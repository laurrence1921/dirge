## Build: cargo test (2141 pass), cargo check for verify. Zero warnings.
Package name `dirge-agent`, binary `dirge`. No lib target — use `cargo test -- <filter>` not `cargo test --lib`.
§
## AbortSignal dual flags

LOOP-4: `AbortSignal` in `tool.rs` has two `Arc<AtomicBool>`: `cancelled` (hard abort, tools check) and `interjected` (graceful stop at turn boundary, loop checks in `run.rs`). `into_agent_runner()` in `integration.rs` calls `signal.interject()`, not `signal.cancel()`, for UI interject signals.
§
## Doom-loop: HashMap per-key counter + FIFO ring, window 32, check-before-track (threshold 2)

checker.rs: `repeat_counts: HashMap<String, u32>` keyed `"{tool}\x00{input}"`. track_doom_loop bumps counter + pushes FIFO; eviction pops front + decrements. is_doom_loop checks count >= 2 BEFORE tracking (counter reflects previous only, not current). Window 32 (was 16) defeats 14-call decoy-gap. `repeat_counts.clear()` on set_working_dir.
§
Terminal tab title: OSC 0 escape `\x1b]0;{title}\x07` written to `terminal.backend_mut()` after frame draw in `tui_redraw()`. Dirty-check against cached title to skip redundant writes. See `experimental-ui-terminal-tab` feature.
§
## Compaction system (PRs #220-#225, commit ed91710)

All three Claude Code gaps now implemented:
1. Circuit breaker: MAX_CONSECUTIVE_COMPACTION_FAILURES=3, SummaryOutcome enum. After 3 fails skips LLM summarizer (prune still runs).
2. Aggressive prune: tiered_result_cap 3000→1000 tokens at >60% context. Pure fn in compression.rs.
3. Post-compaction file restore: FileTouchTracker::working_files() sorted overlap. Guards: MAX_READ_BYTES=2MB, RESTORE_CEILING=0.50.
4. Snip feedback loop: cap_oversized_tool_results_counted → (messages, freed). If >10% window freed, skip post-response NORMAL fold.
5. Report enrichment: CompactionKind enum (PruneOnly/PruneAndSummary/PruneAndFailedSummary/PruneSummarizerDisabled) in event.rs:35.
Key types: SummaryOutcome{Succeeded(usize),Failed,Skipped} at run.rs:184.
