//! The teleprompter's words: the script as it is shown (numbers as digits,
//! `(pause)` marks and `---` breaks folded in) and the matching of what the
//! reader says against it.

/// How many recent heard words are matched against the script.
const HEARD_TAIL: usize = 6;
/// How far back from a candidate word the heard words may match.
const MATCH_SPAN: usize = 12;
/// How far behind and ahead of the current word a match is looked for.
const LOOK_BEHIND: usize = 8;
const LOOK_AHEAD: usize = 24;
/// Going back fewer words than this is taken as a repeated word, not a restart.
const MIN_STEP_BACK: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct PromptWord {
    pub text: String,
    /// `text` reduced for matching against speech.
    pub key: String,
    /// The script asks for a pause after this word.
    pub pause: bool,
}

pub fn prompt_words(text: &str) -> Vec<PromptWord> {
    let mut raw: Vec<Raw> = Vec::new();
    let mut pause_after = Vec::new();
    for token in text.split_whitespace() {
        let lower = token.to_lowercase();
        if token.chars().all(|c| c == '-' || c == '—') {
            continue;
        }
        if matches!(lower.trim_matches(|c: char| !c.is_alphanumeric()), "pause")
            && (lower.starts_with('(') || lower.starts_with('['))
        {
            if let Some(last) = pause_after.last_mut() {
                *last = true;
            }
            continue;
        }
        for part in split_token(token) {
            raw.push(part);
            pause_after.push(false);
        }
    }

    let mut words = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let (text, used) = match spoken_number(&raw[index..]) {
            Some((number, used)) => (
                format!(
                    "{}{}{}",
                    raw[index].lead,
                    number,
                    raw[index + used - 1].trail
                ),
                used,
            ),
            None => (raw[index].text(), 1),
        };
        words.push(PromptWord {
            key: match_key(&text),
            pause: pause_after[index + used - 1],
            text,
        });
        index += used;
    }
    words
}

/// A word reduced to lowercase letters, digits and inner points.
pub fn match_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric() || *c == '.')
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

/// Match keys for what speech recognition heard. Its tokens split figures
/// ("76", ",000") and punctuation off, so they are joined back up and read like
/// the script, spoken numbers included.
pub fn heard_keys(tokens: &[String]) -> Vec<String> {
    let mut text = String::new();
    for token in tokens {
        let glued = token
            .chars()
            .next()
            .is_some_and(|first| !first.is_alphanumeric());
        if !glued && !text.is_empty() {
            text.push(' ');
        }
        text.push_str(token);
    }
    prompt_words(&text)
        .into_iter()
        .map(|word| word.key)
        .filter(|key| !key.is_empty())
        .collect()
}

/// Where the reader is after saying `heard` (newest last): the index of the
/// word after the last one they said, when it can be found near `position`.
pub fn follow_heard(script: &[String], position: usize, heard: &[String]) -> Option<usize> {
    let heard: Vec<&str> = heard
        .iter()
        .map(String::as_str)
        .filter(|word| !word.is_empty())
        .collect();
    let tail = &heard[heard.len().saturating_sub(HEARD_TAIL)..];
    let last = *tail.last()?;
    let from = position.saturating_sub(LOOK_BEHIND);
    let to = (position + LOOK_AHEAD).min(script.len());
    let mut best: Option<(usize, usize)> = None;
    for end in from..to {
        if !similar(&script[end], last) {
            continue;
        }
        let window = &script[end.saturating_sub(MATCH_SPAN - 1)..=end];
        let score = common_run(window, tail);
        let accepted =
            score >= 2 || (score == 1 && tail.len() == 1 && end >= position && end < position + 3);
        let back = position.saturating_sub(end + 1);
        if !accepted || (back > 0 && (score < 3 || back < MIN_STEP_BACK)) {
            continue;
        }
        let distance = end.abs_diff(position);
        let better = best.is_none_or(|(best_end, best_score)| {
            score > best_score || (score == best_score && distance < best_end.abs_diff(position))
        });
        if better {
            best = Some((end, score));
        }
    }
    best.map(|(end, _)| end + 1)
}

/// The longest common subsequence of the script window and the heard words,
/// counting only matches that end on both last words.
fn common_run(window: &[String], heard: &[&str]) -> usize {
    let mut table = vec![vec![0usize; heard.len() + 1]; window.len() + 1];
    for (i, word) in window.iter().enumerate() {
        for (j, said) in heard.iter().enumerate() {
            table[i + 1][j + 1] = if similar(word, said) {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }
    table[window.len()][heard.len()]
}

fn similar(script: &str, heard: &str) -> bool {
    if script == heard {
        return true;
    }
    let shorter = script.len().min(heard.len());
    if shorter < 4 {
        return false;
    }
    script.starts_with(heard) || heard.starts_with(script) || edit_distance(script, heard) <= 1
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = if ca == *cb {
                previous
            } else {
                1 + previous.min(row[j]).min(row[j + 1])
            };
            previous = current;
        }
    }
    row[b.len()]
}

#[derive(Debug, Clone)]
struct Raw {
    lead: String,
    core: String,
    trail: String,
}

impl Raw {
    fn text(&self) -> String {
        format!("{}{}{}", self.lead, self.core, self.trail)
    }

    fn word(&self) -> String {
        self.core.to_lowercase()
    }
}

/// Splits off surrounding punctuation, and splits hyphenated numbers like
/// "twenty-eight" into their words.
fn split_token(token: &str) -> Vec<Raw> {
    let start = token
        .find(|c: char| c.is_alphanumeric())
        .unwrap_or(token.len());
    let end = token
        .rfind(|c: char| c.is_alphanumeric())
        .map_or(start, |end| {
            end + token[end..].chars().next().map_or(1, char::len_utf8)
        });
    let (lead, core, trail) = (&token[..start], &token[start..end], &token[end..]);
    let parts: Vec<&str> = core.split('-').collect();
    if parts.len() > 1
        && parts
            .iter()
            .all(|part| number_word(&part.to_lowercase()).is_some())
    {
        let last = parts.len() - 1;
        return parts
            .iter()
            .enumerate()
            .map(|(index, part)| Raw {
                lead: if index == 0 {
                    lead.to_string()
                } else {
                    String::new()
                },
                core: (*part).to_string(),
                trail: if index == last {
                    trail.to_string()
                } else {
                    String::new()
                },
            })
            .collect();
    }
    vec![Raw {
        lead: lead.to_string(),
        core: core.to_string(),
        trail: trail.to_string(),
    }]
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NumberWord {
    Unit(u64),
    Tens(u64),
    Hundred,
    Scale(u64),
}

fn number_word(word: &str) -> Option<(NumberWord, bool)> {
    const UNITS: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const UNIT_ORDINALS: [&str; 20] = [
        "zeroth",
        "first",
        "second",
        "third",
        "fourth",
        "fifth",
        "sixth",
        "seventh",
        "eighth",
        "ninth",
        "tenth",
        "eleventh",
        "twelfth",
        "thirteenth",
        "fourteenth",
        "fifteenth",
        "sixteenth",
        "seventeenth",
        "eighteenth",
        "nineteenth",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    const TENS_ORDINALS: [&str; 8] = [
        "twentieth",
        "thirtieth",
        "fortieth",
        "fiftieth",
        "sixtieth",
        "seventieth",
        "eightieth",
        "ninetieth",
    ];
    if let Some(value) = UNITS.iter().position(|unit| *unit == word) {
        return Some((NumberWord::Unit(value as u64), false));
    }
    if let Some(value) = UNIT_ORDINALS.iter().position(|unit| *unit == word) {
        return Some((NumberWord::Unit(value as u64), true));
    }
    if let Some(index) = TENS.iter().position(|tens| *tens == word) {
        return Some((NumberWord::Tens(20 + index as u64 * 10), false));
    }
    if let Some(index) = TENS_ORDINALS.iter().position(|tens| *tens == word) {
        return Some((NumberWord::Tens(20 + index as u64 * 10), true));
    }
    match word {
        "hundred" => Some((NumberWord::Hundred, false)),
        "hundredth" => Some((NumberWord::Hundred, true)),
        "thousand" => Some((NumberWord::Scale(1_000), false)),
        "thousandth" => Some((NumberWord::Scale(1_000), true)),
        "million" => Some((NumberWord::Scale(1_000_000), false)),
        "billion" => Some((NumberWord::Scale(1_000_000_000), false)),
        _ => None,
    }
}

struct Parsed {
    value: u64,
    used: usize,
    ordinal: bool,
    /// Ends at a word whose punctuation closes the number.
    closed: bool,
    /// Only the tens and units below 100 were said.
    small: bool,
}

/// Number words at the start of `raw` as one figure, with how many words it
/// took. A lone "one" stays a word ("one of the pages").
fn spoken_number(raw: &[Raw]) -> Option<(String, usize)> {
    let mut parsed = whole_number(raw)?;
    let mut text = String::new();
    let mut decimals = String::new();

    if !parsed.ordinal
        && !parsed.closed
        && parsed.small
        && (11..=20).contains(&parsed.value)
        && let Some(rest) = whole_number(&raw[parsed.used..])
        && rest.small
        && (10..=99).contains(&rest.value)
    {
        text = (parsed.value * 100 + rest.value).to_string();
        parsed.used += rest.used;
        parsed.closed = rest.closed;
        parsed.ordinal = rest.ordinal;
    }

    if text.is_empty()
        && !parsed.ordinal
        && !parsed.closed
        && raw
            .get(parsed.used)
            .is_some_and(|word| word.word() == "point")
    {
        let mut at = parsed.used + 1;
        while let Some(word) = raw.get(at) {
            match number_word(&word.word()) {
                Some((NumberWord::Unit(digit), false)) if digit < 10 => {
                    decimals.push_str(&digit.to_string());
                    at += 1;
                    if !word.trail.is_empty() {
                        break;
                    }
                }
                _ => break,
            }
        }
        if !decimals.is_empty() {
            parsed.closed = !raw[at - 1].trail.is_empty();
            parsed.used = at;
        }
    }

    if text.is_empty() {
        text = grouped(parsed.value);
        if !decimals.is_empty() {
            text = format!("{text}.{decimals}");
        }
    }
    if parsed.used == 1 && parsed.value == 1 && decimals.is_empty() && !parsed.ordinal {
        return None;
    }
    let date = raw.get(parsed.used).is_some_and(|word| word.word() == "of");
    if parsed.ordinal && parsed.used == 1 && parsed.value < 10 && !date {
        return None;
    }
    if parsed.ordinal {
        text.push_str(ordinal_suffix(parsed.value));
    } else if !parsed.closed {
        match raw.get(parsed.used).map(Raw::word).as_deref() {
            Some("percent") => {
                text.push('%');
                parsed.used += 1;
            }
            Some("per")
                if raw
                    .get(parsed.used + 1)
                    .is_some_and(|word| word.word() == "cent") =>
            {
                text.push('%');
                parsed.used += 2;
            }
            Some("pounds") => {
                text.insert(0, '£');
                parsed.used += 1;
            }
            _ => {}
        }
    }
    Some((text, parsed.used))
}

fn whole_number(raw: &[Raw]) -> Option<Parsed> {
    let mut total = 0u64;
    let mut group = 0u64;
    let mut used = 0usize;
    let mut said = false;
    let mut small = true;
    let mut last: Option<NumberWord> = None;
    let mut ordinal = false;
    let mut closed = false;

    while let Some(word) = raw.get(used) {
        let lower = word.word();
        let next_is_number = || {
            raw.get(used + 1)
                .and_then(|next| number_word(&next.word()))
                .is_some_and(|(kind, _)| matches!(kind, NumberWord::Unit(_) | NumberWord::Tens(_)))
        };
        if lower == "and"
            && said
            && matches!(last, Some(NumberWord::Hundred | NumberWord::Scale(_)))
            && next_is_number()
            && word.trail.is_empty()
        {
            used += 1;
            continue;
        }
        if lower == "a"
            && used == 0
            && raw.get(1).is_some_and(|next| {
                matches!(
                    number_word(&next.word()),
                    Some((NumberWord::Hundred | NumberWord::Scale(_), _))
                )
            })
        {
            group = 1;
            said = true;
            small = false;
            used += 1;
            continue;
        }
        let Some((kind, is_ordinal)) = number_word(&lower) else {
            break;
        };
        match kind {
            NumberWord::Unit(value) => {
                let fits = group.is_multiple_of(100)
                    || (matches!(last, Some(NumberWord::Tens(_)))
                        && group.is_multiple_of(10)
                        && value < 10);
                if !fits || (said && matches!(last, Some(NumberWord::Unit(_)))) {
                    break;
                }
                group += value;
            }
            NumberWord::Tens(value) => {
                if !group.is_multiple_of(100) || (said && matches!(last, Some(NumberWord::Tens(_))))
                {
                    break;
                }
                group += value;
            }
            NumberWord::Hundred => {
                if group == 0 || group >= 100 {
                    break;
                }
                group *= 100;
                small = false;
            }
            NumberWord::Scale(scale) => {
                if group == 0 && total == 0 {
                    break;
                }
                total += group.max(1) * scale;
                group = 0;
                small = false;
            }
        }
        said = true;
        last = Some(kind);
        used += 1;
        if is_ordinal {
            ordinal = true;
            closed = !word.trail.is_empty();
            break;
        }
        if !word.trail.is_empty() {
            let comma_inside = word.trail == ","
                && matches!(kind, NumberWord::Scale(_))
                && raw
                    .get(used)
                    .and_then(|next| number_word(&next.word()))
                    .is_some_and(|(kind, _)| !matches!(kind, NumberWord::Scale(_)));
            if !comma_inside {
                closed = true;
                break;
            }
        }
    }
    said.then_some(Parsed {
        value: total + group,
        used,
        ordinal,
        closed,
        small: small && used <= 2,
    })
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    if value < 1_000 {
        return digits;
    }
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn ordinal_suffix(value: u64) -> &'static str {
    match (value % 10, value % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str) -> Vec<String> {
        prompt_words(text)
            .into_iter()
            .map(|word| word.text)
            .collect()
    }

    #[test]
    fn spoken_numbers_become_figures() {
        assert_eq!(
            shown(
                "Clicks came in at four hundred and twenty. (pause) That's down twenty-eight percent."
            ),
            [
                "Clicks", "came", "in", "at", "420.", "That's", "down", "28%."
            ]
        );
        assert_eq!(
            shown("from zero point five, to zero point five five percent."),
            ["from", "0.5,", "to", "0.55%."]
        );
        assert_eq!(
            shown("Revenue was down, (pause) at five thousand, one hundred and fifty pounds."),
            ["Revenue", "was", "down,", "at", "£5,150."]
        );
        assert_eq!(
            shown(
                "It sent a hundred and thirty sessions, and around eight thousand eight hundred pounds"
            ),
            ["It", "sent", "130", "sessions,", "and", "around", "£8,800"]
        );
        assert_eq!(
            shown("the eighteenth of September, to just over seventy-six thousand."),
            [
                "the",
                "18th",
                "of",
                "September,",
                "to",
                "just",
                "over",
                "76,000."
            ]
        );
        assert_eq!(
            shown(
                "One to watch is ChatGPT in twenty twenty-six, slipped by one place, thirteen point seven"
            ),
            [
                "One", "to", "watch", "is", "ChatGPT", "in", "2026,", "slipped", "by", "one",
                "place,", "13.7"
            ]
        );
        assert_eq!(
            shown("Two pages, five areas, GA4"),
            ["2", "pages,", "5", "areas,", "GA4"]
        );
    }

    #[test]
    fn pauses_and_breaks_are_folded_in() {
        let words = prompt_words("Hello, (pause) and welcome.\n\n--- Let's start");
        let texts: Vec<_> = words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, ["Hello,", "and", "welcome.", "Let's", "start"]);
        assert!(words[0].pause);
        assert!(!words[1].pause);
        assert_eq!(words[2].key, "welcome");
    }

    fn keys(text: &str) -> Vec<String> {
        prompt_words(text)
            .into_iter()
            .map(|word| word.key)
            .collect()
    }

    #[test]
    fn heard_words_move_the_reader_on() {
        let script = keys(
            "Clicks came in at four hundred and twenty. That's down twenty-eight percent. \
             Impressions fell thirty-five percent, to just over seventy-six thousand.",
        );
        let heard = |text: &str| keys(text);
        assert_eq!(follow_heard(&script, 0, &heard("clicks came in")), Some(3));
        assert_eq!(
            follow_heard(&script, 3, &heard("came in at 420 that's down")),
            Some(7)
        );
        assert_eq!(
            follow_heard(&script, 7, &heard("down 28% impression fell")),
            Some(10)
        );
        assert_eq!(follow_heard(&script, 7, &heard("banana")), None);
        assert_eq!(follow_heard(&script, 9, &heard("clicks came in")), Some(3));
        assert_eq!(follow_heard(&script, 9, &heard("came in")), None);
    }

    #[test]
    fn heard_figures_are_joined_back_up() {
        let tokens: Vec<String> = [
            "to", "just", "over", "76", ",000", ".", "From", "0", ".5", "%",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            heard_keys(&tokens),
            ["to", "just", "over", "76000", "from", "0.5"]
        );
        let spoken: Vec<String> = ["zero", "point", "five", "percent"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(heard_keys(&spoken), ["0.5"]);
    }

    #[test]
    fn small_ordinals_stay_words_unless_a_date() {
        assert_eq!(
            shown("First, the third of May"),
            ["First,", "the", "3rd", "of", "May"]
        );
    }
}
