# Custom fork map (jakemulholland-code/Cap)

Quick index for working on this fork. Search by the symbol name; line numbers
are hints that drift. Read only the region you need from the big files.

## Setup and commands (Windows)

- Fork: `origin` = jakemulholland-code/Cap, `upstream` = capsoftware/cap. Work on `custom`.
- The shipping app is `apps/desktop-gpui`, a **standalone Cargo workspace** with its own
  `rust-toolchain.toml` (stable). Run cargo from that folder, never the repo root (root pins 1.88 and
  excludes it). It needs `../zed-cap` (made by `scripts/prepare-gpui-dependency.sh`).
- PowerShell env for cargo: `$env:Path = "$env:USERPROFILE\.cargo\bin;" + [Environment]::GetEnvironmentVariable("Path","User") + ";" + [Environment]::GetEnvironmentVariable("Path","Machine")`
- Build: `cargo build` (~15-40s incremental). Tests: `cargo test --bin cap-gpui <filter>`
  (e.g. `editor_auto_edit`, `editor_template`, `editor_` for all editor tests).
- Format only touched files: `rustfmt --edition 2024 <files>`. **Don't** run `cargo fmt`: it rewrites
  unrelated upstream files.
- Run: `scripts/start-local-cap.cmd` (`rebuild` arg rebuilds first). The exe is locked while running, so close it
  before `cargo build`. Uses `CAP_GPUI_APP_DATA_DIR=target/gpui-dev-data` (separate from installed Cap).
- Native DLLs (ffmpeg/dxc/onnx) come from `cap-setup` into `target/debug` and get copied next to the exe.
- Smart App Control must stay off or build scripts are blocked (os error 4551).
- The scratchpad temp path is too long for native Windows tools; use `$TEMP\<short>` for downloads.
- AGENTS.md rules: no narrating comments, tabs plus double quotes in TS, clippy-clean Rust.

## Custom features

### Auto edit (cut silent and idle sections)
- `apps/desktop-gpui/src/editor_auto_edit.rs`: pure logic plus tests. `AutoEditSettings`, `AutoEditTransition`,
  `RetakeMode`, `CutReason` (Silence/Noise/Retake). Entry point `detect_cuts` = `detect_empty_sections`
  (10 bins/s from waveform dB tables plus activity; threshold from `measure_levels` (Otsu split, midway between
  room and speech) unless a fixed one is picked; `noise_mask` folds isolated bursts with no transcript word,
  or much louder than speech, into the silence) + `retake_cuts` (`retake_ranges`: spoken cues in
  `RETAKE_CUES` roll back via fuzzy `matching_attempt` or `sentence_start`; `repeated_lines` catches a
  restarted line without a cue), then `merge_cuts`. `transcript_words` flattens caption segments into
  per-recording words. `cut_output_range` maps a cut to output time. `apply_cuts`, `load_recording_activity`.
  Tests include a real 36 s take (`RECORDED_LEVELS`/`RECORDED_WORDS`); the mic waveform is mean-abs dBFS where
  exact -60 means digital zero, and a typical mic speaks near -50 with room tone near -95.
- `apps/desktop-gpui/src/editor_window/auto_edit.rs`: `AutoEditPanel`, `toggle_auto_edit`,
  `load_auto_edit_transcript` (transcribes with the downloaded caption model when the project has no captions,
  kept in `auto_edit_transcript`, not written to the project), `refresh_auto_edit_preview`, preview
  (`step_auto_edit_cut`, `play_auto_edit_cut`, `advance_auto_edit_preview` hooked into the `play_until` stop in
  `playhead_changed`), `apply_auto_edit`, `render_auto_edit_overlays`, `render_auto_edit_popover`.
- Icons must be registered in `src/assets.rs` `ICONS` or gpui draws nothing (this is why `sparkles.svg` was blank).
- Hooks in `editor_window.rs`: fields `auto_edit*` (~1466); `refresh_auto_edit_preview()` at the end of
  `rebuild_timeline` and in `set_waveforms`; Esc handler near `self.clip_speed.is_some() && ... "escape"`;
  button `"transport-auto-edit"` in `render_transport`; overlays and popover are mounted next to the
  `render_clip_speed_*` calls.

### Guided recording (Claude Motion video + transcript, recorded section by section)
- Flow: tray "Guided Recording..." -> pick video + transcript -> check the split -> "Record sections" ->
  record a take per section (camera + mic) -> "Build project" -> opens in the editor with Studio Sound on.
- `apps/desktop-gpui/src/guided_recording.rs`: pure logic plus tests. `parse_script` reads the transcript
  (`# Title`, then `## m:ss Heading` + spoken text; text before the first `##` is ignored). `sample_video`
  decodes once to 64x36 grey signatures at 10/s (`VideoSamples`); `scene_cuts` finds slide changes (peaks of
  change across 3 samples, refined to the first sample showing the new slide). `plan_sections` keeps written
  timestamps, fills missing ones by read time (`READ_WORDS_PER_MINUTE` 140), then snaps each start to the
  strongest cut within 1.5 s (6 s if estimated). `GuidedSession` (saved to `<app data>/guided/<slug>/session.json`,
  takes in `takes/section-NN-take-N.cap`), `move_boundary`, `neighbouring_cut`, `add_take`, `ready_to_build`.
  `Take` (`lead_secs` = recording start to slide start, `duration`, `timing` = `[take secs, source secs]` points,
  linear between; empty = normal speed), `GuidedSection::chosen_take` (newest unless picked). `timing_source` /
  `timing_take` map both ways. `SegmentPlan` (lead hold, then the take's timing, clamped to the section;
  `source_time`, `segment_time`). `slide_rate` fits a slide to the read at a wpm (0.4x-1.5x). `detect_beats` finds
  step points (motion rising after 3 still samples; stored as `GuidedSession::beats`, `next_beat`,
  `session_beats` fills old sessions). `prepare_session`.
- `apps/desktop-gpui/src/guided_media.rs`: `VideoReader` (decode from a time, convert/scale), `SlideClock` (live
  slide time: rate, wait at a step point, hold at the end, records timing points), `Pace` (Linear/Timed/Live),
  `Player` (thread showing pictures by their `Pace`, optional rodio sound), `write_segment_display` (H.264 via
  `import::open_h264_encoder`), `segment_audio` / `write_segment_audio` (the slide's sounds are found between
  silences and each starts when its moment is shown, unstretched), `mix`. Ignored test `real_segment_render`.
- `apps/desktop-gpui/src/guided_build.rs`: `build_project` writes `<recordings>/<title>.cap`: segment N =
  rendered slide `display.mp4` (start_time = camera start), copied take camera + mic (their own start_times),
  `system_audio.wav`; timeline clip N = [lead, lead + take]; config copied from the first take, then
  `recording::enable_studio_sound`. `take_media` for review. Ignored test `real_build_from_fake_takes`.
- `apps/desktop-gpui/src/guided_window.rs`: `GuidedRecordingWindow` (stages Choose/Preparing/Ready/Record),
  split list with thumbnails and boundary moves, recent sessions.
- `apps/desktop-gpui/src/guided_window/recorder.rs`: `Recorder`/`Phase`. Takes are `recording::start` with
  `ScreenCaptureTarget::CameraOnly` + the main window's camera/mic from `Feeds`, stopped with
  `ActiveRecording::stop`, cancelled with `cancel_and_delete`. 3 s countdown, then the slide plays and the
  script scrolls at the wpm setting. The slide runs on a `SlideClock` at `slide_speed` (hand-picked, else fitted to
  the wpm, normal in step mode); step mode waits at each beat. Keys: Space record/stop, Esc cancel, → next
  animation while recording (next section when idle), ← previous section, ↑/↓ slide speed (live while recording).
- Wiring: `app_windows::open_guided_recording` / `guided_recording_closed` (field `guided`, skipped in
  `own_windows`), `TrayItem::GuidedRecording`, `editor_clips::decode_thumbnail_frame` and import's H.264 helpers
  are `pub(crate)`. Harness: `CAP_GPUI_AUTO_TRAY=guided_recording`, `CAP_GPUI_AUTO_GUIDED=<session.json>[,record]`,
  snapshot targets `guided` / `editor` (snapshots work on macOS only).
- Hamilton sample: slide changes land on whole seconds; most sections read longer at 140 wpm than their slide.

### Paramount Digital template
- `apps/desktop-gpui/src/editor_template.rs`: pure logic plus tests.
  - Brand constants: Sora/Fraunces, `#1a1a1a`, `#008475`, `[251,251,252]`.
  - Cards: `insert_card` (kicker plus heading, fullscreen hold), `make_room` (ripples output-timed tracks).
  - Section entry point: `add_section_card`.
  - Card detection: `card_windows` (groups cards by start; merged holds can't split them), `section_windows`,
    `renumber_sections`.
  - `apply_template` adds the intro/outro, watermark (`add_watermark`), `style_frame` and `style_captions`.
  - `write_logo_png` rasterises `assets/brand/paramount-logo.svg` into `content/images/paramount-digital-logo.png`.
  - Intro/outro are identified by the text `INTRO_KICKER` / `OUTRO_URL`; images by name.
- `apps/desktop-gpui/src/editor_window/template.rs`: `add_section_at_playhead`, `apply_paramount_template`,
  `sync_template_lanes`.
- Buttons: `"paramount-template"` in `render_header` (after Presets) and `"transport-add-section"` in
  `render_transport`.
- Renderer change: `crates/rendering/src/layers/mod.rs` `caption_font_family` (interned `Family::Name`), used
  by `captions.rs` and `keyboard.rs`.
- Fonts are installed per-user in `%LOCALAPPDATA%\Microsoft\Windows\Fonts` (Sora 300-700, Fraunces 600/700/900/700i).
  The source logo is at `../gmc-limit-checker/templates/_paramount_logo.svg`.

## Editor core (apps/desktop-gpui/src)

`editor_window.rs` (~14k lines, `EditorWindow`):
- Write path: `edit(|timeline| -> bool)` ~2376 and `edit_timeline_project(|project| -> bool)` ~2394. Each makes one
  undo step and leads to `project_changed_inner` ~2480 (history.record, rebuild_timeline, publish, save).
- `rebuild_timeline` ~2295, `set_waveforms` ~2332, `undo` ~2606, `note_edit(reason, track)` (log only).
- UI: `render_header` ~9683 (Presets, Template), `render_transport` ~10398 (Split, Auto edit, Add section,
  zoom), `render_timeline_body` ~10975, `render_clip_speed_popover` ~8512 (popover/chip pattern).
- Patterns: `generate_auto_zoom` ~5755 (async background load, then edit); `commit_image_import` ~7774
  (image sizing via `frame_layout.output_size`, `self.tracks.*` lane bump, `Selection::single`);
  `clip_menu_placement` ~1706; keys are matched by hand in the key-down handler (~3498).
- Submodules in `editor_window/` (`use super::*;`, `impl EditorWindow`): auto_edit, template, frame, loading,
  scenes, snap.
- State: `self.project` (ProjectConfiguration), `self.timeline` (TimelineModel: `clips`, `mic_waveforms`,
  `system_waveforms`), `self.playhead`, `self.view.transform`, `self.tracks`, `self.project_path`,
  `self.summary()` (`pretty_name`).

`editor_edits.rs`: `ProjectHistory` ~149, `delete_clip_segments` ~1208 (ripples, keeps the last clip),
`effective_to_output` ~1270, `default_text_segment` ~1698, `insert_text_segment` ~1785, `ensure_timeline`
~1929, `split_clip_segment(timeline, output_time, Some(idx))` ~2242, `insert_image_segment` ~4996,
`used_lane_count`, tests ~2861.

`editor_clips.rs`: `ripple_track` ~179, `ripple_keyboard_track`, `drop_clip_transition` ~207,
`add_clip_transition` ~251 (custom).

`editor_timeline.rs`: waveform tables are dBFS mean-abs at 10 values/s, -60 floor (`waveform_peaks` ~529);
`clip_timeline_offsets` ~1640; `TimelineModel.mic_waveforms` ~1287.

Other: `import.rs` `import_editor_image` ~2270; `presets.rs` `apply_preset` ~155; `main.rs` mod list; i18n is
not used by editor strings (plain literals).

## Project config (crates/project/src/configuration.rs)

`TimelineSegment` ~932 (source-time start/end, `recording_clip`, `timescale`), `TextLayout` ~1208,
`TextSegment` ~1222, `ImageSegment` ~1743 (path relative to the project dir), `ClipTransition` ~1812
(CrossFade | FadeThroughBlack), `TimelineConfiguration` ~1842, `effective_transition` ~1932, `hold_windows`
~1974 (fullscreen text = a pause that inserts output time), `BackgroundConfiguration` ~353 (`Color` is `[u16;3]`),
`CaptionSettings` ~2330, `KeyboardSettings` ~2451, `ProjectConfiguration` ~2658.
Cursor/keyboard events: `crates/project/src/cursor.rs` (`CursorEvents`), `keyboard.rs`, `meta.rs`
(`keyboard_events`).
Renderer: `crates/rendering/src/layers/{text,captions,keyboard,image}.rs`, `transition.rs`, `takeover.rs`.
