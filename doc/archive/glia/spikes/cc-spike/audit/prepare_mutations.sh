#!/usr/bin/env bash
set -euo pipefail

source_dir="$(cd "$(dirname "$0")/.." && pwd)"
variants_dir="$(cd "$source_dir/.." && pwd)/cc-spike-mutations"
mkdir -p "$variants_dir"
mkdir -p "$variants_dir/ownership-spike"
rsync -a --exclude target --exclude .context \
  "$source_dir/../ownership-spike/" "$variants_dir/ownership-spike/"

make_variant() {
  local name="$1"
  local dst="$variants_dir/$name"
  mkdir -p "$dst"
  rsync -a \
    --exclude target \
    --exclude .context \
    --exclude fuzz/target \
    --exclude audit/prepare_mutations.sh \
    "$source_dir/" "$dst/"
}

make_variant m01_double_subtract
perl -0pi -e 's/cm\.strong\.set\(cm\.strong\.get\(\) - 1\);/cm.strong.set(cm.strong.get() - 1);\n                    cm.strong.set(cm.strong.get() - 1);/' "$variants_dir/m01_double_subtract/src/cc.rs"

make_variant m02_omit_participating
perl -0pi -e 's/MVal::Atom\(a\) => t\.edge\(a\),/MVal::Atom(_a) => {},/' "$variants_dir/m02_omit_participating/src/model.rs"

make_variant m03_skip_scan_restore
perl -0pi -e 's/cm\.strong\.set\(cm\.strong\.get\(\) \+ 1\);\n            if cm\.color/let _ = cm;\n            if cm.color/' "$variants_dir/m03_skip_scan_restore/src/cc.rs"

make_variant m04_remove_freed_guard
perl -0pi -e 's/if !m\.freed\.get\(\) \{\n            m\.freed\.set\(true\);/if true {\n            m.freed.set(true);/' "$variants_dir/m04_remove_freed_guard/src/cc.rs"

make_variant m05_drop_before_freed
perl -0pi -e 's/        m\.freed\.set\(true\);\n    \}\n    \/\/ 4c/    }\n    \/\/ 4c/' "$variants_dir/m05_drop_before_freed/src/cc.rs"
perl -0pi -e 's/unsafe \{ \(w\.meta\(\)\.vt\.drop_value\)\(w\) \};/unsafe { (w.meta().vt.drop_value)(w) };\n        w.meta().freed.set(true);/' "$variants_dir/m05_drop_before_freed/src/cc.rs"

make_variant m06_dealloc_before_cascade
perl -0pi -e 's/unsafe \{ \(m\.vt\.drop_value\)\(e\) \};/unsafe { (m.vt.dealloc)(e) };\n            unsafe { (m.vt.drop_value)(e) };/' "$variants_dir/m06_dealloc_before_cascade/src/cc.rs"

make_variant m07_dead_candidate_parked
perl -0pi -e 's/unsafe \{ \(m\.vt\.dealloc\)\(e\) \};\n        \} else if m\.color/m.buffered.set(true);\n            ROOTS.with(|b| b.borrow_mut().push(e));\n        } else if m.color/' "$variants_dir/m07_dead_candidate_parked/src/cc.rs"

make_variant m08_duplicate_buffer
perl -0pi -e 's/else if !m\.freed\.get\(\) && !m\.buffered\.get\(\)/else if !m.freed.get()/' "$variants_dir/m08_duplicate_buffer/src/cc.rs"

make_variant m09_skip_precompensation
perl -0pi -e 's/let mut bump = \|c: ErasedCc\| \{\n            let cm = c\.meta\(\);\n            cm\.strong\.set\(cm\.strong\.get\(\) \+ 1\);\n        \};/let mut bump = |_c: ErasedCc| {};/' "$variants_dir/m09_skip_precompensation/src/cc.rs"

make_variant m10_whole_box_mut
perl -0pi -e 's/let b = e\.0\.as_ptr\(\) as \*mut CcBox<Self>;\n                    unsafe \{ ManuallyDrop::drop\(&mut \(\*b\)\.value\) \};/let b: \&mut CcBox<Self> = unsafe { \&mut *(e.0.as_ptr() as *mut CcBox<Self>) };\n                    unsafe { ManuallyDrop::drop(\&mut b.value) };/' "$variants_dir/m10_whole_box_mut/src/cc.rs"
perl -0pi -e 's/        \/\/ Re-derive after the value drop:[^\n]*\n        \/\/ call that mutates the allocation\.\n        let m = e\.meta\(\);\n/        \/\/ MUTATION: retain the pre-drop metadata reference across the whole-box \&mut.\n/' "$variants_dir/m10_whole_box_mut/src/cc.rs"

make_variant m11_continue_after_abort
perl -0pi -e 's/("collect_cycles at a non-safepoint \(live borrow\)"\n                \);)\n                return stats;/$1\n                \/\/ MUTATION: continue into mark\/scan after failed validation./' "$variants_dir/m11_continue_after_abort/src/cc.rs"
perl -0pi -e 's/for r in roots \{/for \&r in \&roots {/' "$variants_dir/m11_continue_after_abort/src/cc.rs"

make_variant m12_report_handle_twice
perl -0pi -e 's/MVal::Atom\(a\) => t\.edge\(a\),/MVal::Atom(a) => { t.edge(a); t.edge(a); },/' "$variants_dir/m12_report_handle_twice/src/model.rs"

make_variant m13_target_is_duplicate_key
perl -0pi -e 's/let handle_addr = c as \*const Cc<T> as usize;/let handle_addr = erased.addr();/' "$variants_dir/m13_target_is_duplicate_key/src/cc.rs"

make_variant m14_ignore_host_root
perl -0pi -e 's/if m\.strong\.get\(\) > 0 \{\n                        scan_black\(e\);/if false \&\& m.strong.get() > 0 {\n                        scan_black(e);/' "$variants_dir/m14_ignore_host_root/src/cc.rs"

# The reviewed implementation is already the m15 mutation: it has no
# COLLECTING/re-entry guard. Preserve an exact copy for the focused audit test.
make_variant m15_allow_reentry

printf '%s\n' "$variants_dir"
