All ten traces verified against the pinned revision. Final report follows.

---

**System:** BEAM/ERTS memory model (per-process generational copying GC + off-heap refcounted binaries + shared literal areas)
**Repository:** https://github.com/erlang/otp
**Revision:** `d459618c98fedca9d5a1e87327e7e62c24fcd67a` (HEAD of master as of fetch on 2026-08-03; commit date 2026-07-31, "Merge branch 'maint'"). All raw links below are pinned to this sha. Line numbers are exact against this revision.
**Files/symbols:** `erts/emulator/internal_doc/GarbageCollection.md`; `erts/emulator/beam/erl_process.h` (Process struct, heap fields L1051-1160); `erl_gc.c` (`minor_collection` L1398, `do_minor` L1639, `major_collection`, `setup_rootset` L2616, `sweep_off_heap` L2996, `erts_garbage_collect_literals` L1136, `erts_deallocate_young_generation` L2101); `erl_gc.h` (`ERTS_IS_GC_DESIRED` L132); `erl_message.c` (`erts_send_message` L698, `erts_try_alloc_message_on_heap` L621, `erts_cleanup_offheap_list` L166); `erl_message.h` (`ErlOffHeap`, `ErlHeapFragment`); `copy.c` (`copy_struct_x`); `erl_binary.h` (`Binary`, `erts_bin_release`); `erl_bits.h` (`ERL_ONHEAP_BINARY_LIMIT`, `BinRef`); `erl_process.c` (`delete_process` L13394); `erl_fun.h` (`ErlFunEntry`, `ErlFunThing`); `erl_nif.c` (`enif_make_resource`, `run_resource_dtor`); `beam_bif_load.c` (`erts_copy_literals_gc` L1119, literal-release protocol comment L1466); `external.c` (`NEW_FUN_EXT` encoding L3959, `enc_internal_pid` L2953); `erts/doc/guides/erl_ext_dist.md`.

**Mechanism:**
Each process owns one contiguous memory block holding heap (grows up) and stack (grows down); GC triggers when they meet. A second block, the old heap, holds tenured data. `high_water` marks where the last GC ended; at the next *minor* GC everything below it (the "mature" region, `mature_size = high_water - mature`) is evacuated to the old heap, everything above to a fresh young heap — two-generation copying with promotion by survival-of-one-GC. The old heap is never scanned during minor GC; safety comes from immutability: old terms cannot point at young terms. A *major* (fullsweep) collection copies both generations into one space; it fires when `gen_gcs >= max_gen_gcs` (`fullsweep_after`), when `F_NEED_FULLSWEEP` is set, when the mature region won't fit in old-heap free space, or on binary pressure (`bin_old_vheap_sz > bin_old_vheap` fails the minor precondition). The rootset is explicit and small: stack, live registers (`objv`), process dictionary, seq-trace token, group leader, parent, exit value/trace, receive markers, NIF saved args, and on-heap message-queue entries. GC is per-process, un-coordinated, and pause-isolated. Message send = deep copy into the receiver's heap (holding its main lock) or into a heap fragment attached later — no mutable structure is ever shared between two process heaps. Two exceptions bypass copying: literals (module constants in a global 1 GB virtual literal area, recognized by pointer range, never copied and never scanned) and off-heap binaries >64 bytes (refcounted `Binary` objects; the heap holds a small `BinRef` node linked into the per-process `off_heap` list; copy = refc++, GC = list sweep + refc--, contents never traced; a "virtual binary heap" counter triggers GC early under binary pressure). Code purge inverts the literal sharing: a collector process makes every process copy the doomed module's literals *into its own heap*, then frees the area once. Process death frees the young block, old block, fragments wholesale, and walks only the off-heap list to drop refcounts. Funs cross the distribution boundary as `NEW_FUN_EXT` (module + index + 16-byte MD5 uniq + free vars), i.e. a durable spec naming code, never serialized code; pids/refs carry node name + creation.

**What problem it solves:** Soft-realtime latency (GC cost proportional to one process's live data, never the system); safe massive concurrency without read/write barriers (shared-nothing invariant); cheap handling of large binaries (no copying on send, O(1) collector cost per reference); zero-cost shared constants (literals); O(1)-ish process teardown; location-transparent distribution (callable references serialize as specs).

**What Glia could borrow:**
- *Durable CID data domain* ← mechanisms 5+6 directly. A CID-backed DAG node is exactly a refcounted off-heap `Binary`: process-local executable heap holds a tiny `BinRef`-like handle threaded on a per-process off-heap list; a future tracing GC sweeps the list and drops refcounts without ever traversing DAG contents — O(1) per reference regardless of a billion-node DAG. The `erts_is_literal` pointer-range trick is the cheapest possible "is this durable? then don't trace" predicate: put all frozen/CAS-rooted data in a reserved address region (or one arena) and the copy/trace loop needs only a range check (copy.c does literally `if (litopt && erts_is_literal(obj,objp)) { don't copy }`). Frozen prelude/module constants = literal areas verbatim; the purge protocol (copy-into-heap-then-free) is the precedent for un-pinning a CAS root that's being dropped while borrowers still exist.
- *Local executable domain* ← mechanisms 1-3, 7. If Glia adds per-process GC for closures/definition owners/capability graphs, BEAM proves a two-generation copying scheme with a high-water mark needs no card tables or write barriers *if executable objects are immutable-once-published* (the old-to-young pointer prohibition "comes naturally... because the terms are immutable"). The explicit, tiny rootset (stack + registers + a handful of named cells) fits a Lisp interpreter perfectly. Wholesale arena drop on process exit (mechanism 7) replaces Rc cycle-leak worries at process granularity.
- *vheap accounting* (5): track "bytes of off-heap CID data pinned by this process" and trigger collection/unpin passes on that pressure, not just heap size — directly applicable even without a tracing GC (an Rc world can use it to schedule cycle detection or handle-table compaction).
- *Capability domain* ← mechanisms 8+9. NIF resources = refcounted opaque handles with destructors run exactly at refc-zero — the model for host capabilities held by Glia values. `NEW_FUN_EXT`/`EXPORT_EXT` is the precedent for durable callable specs: a closure crossing a membrane/node serializes as (module CID, index, uniq hash, captured *values*), never as code or a live pointer — matching Wetware's constructive-grant model.
- *Message passing* (4, 10): copy-on-send between Glia processes with the literal/off-heap exception list gives shared-nothing isolation while huge CID payloads move by refcount, not copy.

**What does not transfer:** SMP machinery (main-lock trylock dance, thread-progress-based literal release, dirty schedulers, atomic refcounts) — Glia is single-threaded, so refcounts are plain `Rc` and "thread progress" collapses to a turn boundary. Mutable-term escape hatches BEAM carefully engineered around (heap fragments exist largely because of concurrent senders). The BEAM assumption that all heap data is copyable — Glia capabilities are *not* structurally copyable across membranes; the EXPORT_EXT path (name, don't copy) must be the rule for caps, not the exception. Fun entries being purge-managed rather than refcounted only works because BEAM has a global code-staging protocol.

**Complexity introduced:** Two heaps + high-water bookkeeping per process; move-markers forbid concurrent readers of a heap mid-GC; off-heap list discipline (every handle must be threaded at copy time — BEAM's `copy_struct` does this in one place, Glia would need the same single choke point); the literal purge protocol is a whole distributed-ish state machine (beam_bif_load.c L1466 comment: ~40 lines just to describe it); heap fragments complicate every "where does this term live" question; vheap tuning heuristics (Fibonacci-then-20% growth).

**Confidence:** High. All claims below are quoted from the pinned revision's source/docs; the one OTP-version caveat is noted inline (funs are no longer refcounted on master; the internal doc's "MSO list containing funs" reflects pre-OTP-28 layout — verified `erl_fun.c` has zero `refc` occurrences and `sweep_off_heap`/`erts_cleanup_offheap_list` handle only BIN_REF, magic REF, and external nodes).

---

# Numbered traces

Base URL (pinned): `https://raw.githubusercontent.com/erlang/otp/d459618c98fedca9d5a1e87327e7e62c24fcd67a/`

## 1. Per-process heap

Doc — `erts/emulator/internal_doc/GarbageCollection.md`:
> "Each Erlang process has its own stack and heap which are allocated in the same memory block and grow towards each other. When the stack and the heap meet, the garbage collector is triggered and memory is reclaimed."

Source — `erts/emulator/beam/erl_process.h` (Process struct, exact lines):
```c
1051:    Eterm *htop;                /* Heap top */
1052:    Eterm *stop;                /* Stack top */
1081:    Eterm* heap;                /* Heap start */
1082:    Eterm* hend;                /* Heap end */
1090:    Eterm* abandoned_heap;
1092:    Uint heap_sz;               /* Size of heap in words */
1094:    Uint min_vheap_size;        /* Minimum size of virtual heap (in words). */
1144:    Uint16 gen_gcs;             /* Number of (minor) generational GCs. */
1145:    Uint16 max_gen_gcs;         /* Max minor gen GCs before fullsweep. */
1146:    Eterm *high_water;
1147:    Eterm *old_hend;            /* Heap pointers for generational GC. */
1148:    Eterm *old_htop;
1149:    Eterm *old_heap;
1150:    ErlOffHeap off_heap;        /* Off-heap data updated by copy_struct(). */
1152:    ErlHeapFragment* mbuf;      /* Pointer to heap fragment list */
1155:    Uint mbuf_sz;               /* Total size of heap fragments and message fragments */
1158:    Uint64 bin_vheap_sz;        /* Virtual heap block size for binaries */
1159:    Uint64 bin_old_vheap_sz;    /* Virtual old heap block size for binaries */
1160:    Uint64 bin_old_vheap;       /* Virtual old heap size for binaries */
```
Link: `<base>/erts/emulator/beam/erl_process.h`

## 2. Generational collection

Decision minor vs major — `erl_gc.c` L831:
```c
    if (GEN_GCS(p) < MAX_GEN_GCS(p) && !(FLAGS(p) & F_NEED_FULLSWEEP)) {
        ...
        reds = minor_collection(p, live_hf_end, need + ext_msg_usage, objv, nobj,
				ygen_usage, &reclaimed_now);
        if (reds == -1) {
	    ...
		p->flags |= F_NEED_FULLSWEEP;
	    ...
            goto do_major_collection;
```
Mature region = below high water — `minor_collection`, L1398-1427:
```c
    Eterm *mature = p->abandoned_heap ? p->abandoned_heap : p->heap;
    ...
        high_water = p->high_water;
    ...
    mature_size = high_water - mature;
```
Minor GC precondition (old heap big enough AND binary vheap not over budget) — L1481-1488:
```c
    /*
     * Do a minor collection if there is an old heap and if it
     * is large enough.
     */
    if (OLD_HEAP(p) &&
	((mature_size <= OLD_HEND(p) - OLD_HTOP(p)) &&
	 ((p->bin_old_vheap_sz > p->bin_old_vheap)) ) ) {
```
Promotion inside `do_minor` (L1639), root-scan loop:
```c
                } else if (ErtsInArea(ptr, mature, mature_size)) {
                    move_boxed(ptr,val,&old_htop,g_ptr);
                } else if (ErtsInYoungGen(gval, ptr, oh, oh_size)) {
                    move_boxed(ptr,val,&n_htop,g_ptr);
                }
```
High-water reset: `HIGH_WATER(p) = n_htop;` (do_minor, L1810) and `HIGH_WATER(p) = HEAP_TOP(p);` (major, L1945). `GEN_GCS(p)++` after each minor (L1511); major resets `GEN_GCS(p) = 0` (L1943).

Doc: "no term on the old heap may refer to a term on the young heap... Fortunately, this comes naturally for Erlang because the terms are immutable" and "full sweep... triggered when the size of the area under the high-watermark is larger than the size of the free area of the old heap. It can also be triggered by... `{fullsweep_after, N}` where N is the number of young garbage collections to do before forcing a garbage collection of both young and old heap." (`max_gen_gcs` is the fullsweep_after value, see erl_process.h L1145.)
Link: `<base>/erts/emulator/beam/erl_gc.c`

## 3. The rootset

`erl_gc.c` `setup_rootset` (L2616), each category verbatim:
```c
    roots[n].v  = p->stop;  roots[n].sz = STACK_START(p) - p->stop;  ++n;   /* stack */

    if (p->dictionary != NULL) {
        roots[n].v = ERTS_PD_START(p->dictionary);
        roots[n].sz = ERTS_PD_SIZE(p->dictionary); ++n; }                   /* proc dictionary */

    if (nobj > 0) { roots[n].v = objv; roots[n].sz = nobj; ++n; }           /* live x-registers/args */

    if (is_not_immed(p->seq_trace_token)) { roots[n].v = &p->seq_trace_token; ... }
    if (is_not_immed(p->group_leader))    { roots[n].v = &p->group_leader; ... }
    if (is_not_immed(p->parent))          { roots[n].v = &p->parent; ... }
    if (is_not_immed(p->fvalue))          { roots[n].v = &p->fvalue; ... }  /* exit/throw value */
    if (is_not_immed(p->ftrace))          { roots[n].v = &p->ftrace; ... }  /* stack trace */

    if (p->sig_qs.recv_mrk_blk) {
        roots[n].v = &p->sig_qs.recv_mrk_blk->ref[0];
        roots[n].sz = ERTS_RECV_MARKER_BLOCK_SIZE; n++; }                   /* receive markers */

    if (erts_setup_nfunc_rootset(p, &roots[n].v, &roots[n].sz)) n++;        /* NIF/BIF saved args */

    mp = p->sig_qs.first;                                                   /* on-heap msg queue */
    while (mp) {
        if (ERTS_SIG_IS_INTERNAL_MSG(mp) && !mp->data.attached) {
            roots[n].v = mp->m; roots[n].sz = ERL_MESSAGE_REF_ARRAY_SZ; n++; }
        mp = mp->next; }
```
Rootset type: `typedef struct roots { Eterm* v; /* Pointers to vectors with terms to GC (e.g. the stack). */ Uint sz; } Roots;` with `Roots def[32]` default storage. Doc: "The collector starts by scanning the root-set (stack, registers, etc)."

## 4. Message passing = copying

`erl_message.c` `erts_send_message` (L698), non-SHCOPY (default) path, L771-797:
```c
        msize = size_object_litopt(message, &litarea);
        mp = erts_alloc_message_heap_state(receiver, &receiver_state, receiver_locks,
                                           (msize ...), &hp, &ohp);
        ...
	if (is_not_immed(message))
            message = copy_struct_litopt(message, msize, &hp, ohp, &litarea);
```
Where the copy lands — `erts_try_alloc_message_on_heap` (L621): on the receiver's heap only if the sender can get the receiver's main lock and there is room; otherwise a fragment:
```c
    else if (*plp & ERTS_PROC_LOCK_MAIN) {
    try_on_heap:
	if (((*psp) & ERTS_PSFLGS_VOLATILE_HEAP)
	    || (pp->flags & F_DISABLE_GC)
	    || HEAP_LIMIT(pp) - HEAP_TOP(pp) <= sz) {
	    /*
	     * The heap is either potentially in an inconsistent
	     * state, or not large enough.
	     */
	    ... goto in_message_fragment; }
	*hpp = HEAP_TOP(pp);
	HEAP_TOP(pp) = *hpp + sz;
    ...
    else if (pp && erts_proc_trylock(pp, ERTS_PROC_LOCK_MAIN) == 0) { ... goto try_on_heap; }
    else {
    in_message_fragment:
	...
		bp = new_message_buffer(sz);
		*hpp = &bp->mem[0];
		mp->data.heap_frag = bp;
```
Exceptions to copying: (a) literals — `INITIALIZE_LITERAL_PURGE_AREA(litarea)` + `copy_struct_litopt`; in `copy.c`:
```c
if (litopt && erts_is_literal(obj,objp) && !in_literal_purge_area(objp)) {
    *tailp = obj;   /* reference the literal, do not copy */
    goto L_copy;
}
```
(b) off-heap binaries — see trace 5: `copy_struct` copies only the `BinRef` node and bumps refc. Doc steps: "1. calculate how large the message to be sent is 2. allocate enough space... 3. copy the message payload 4. allocate a message container... 5. insert the message container in the receiver process' message queue."
Links: `<base>/erts/emulator/beam/erl_message.c`, `<base>/erts/emulator/beam/copy.c`

## 5. Off-heap refcounted binaries + vheap

Threshold — `erl_bits.h`:
```c
/* Maximum number of bytes/bits to place in a heap binary.*/
#define ERL_ONHEAP_BINARY_LIMIT 64
#define ERL_ONHEAP_BITS_LIMIT (ERL_ONHEAP_BINARY_LIMIT * 8)
```
(Note: on this master revision the old `ProcBin` is replaced by `BinRef` from the OTP-27 bitstring refactor:)
```c
/** @brief A handle to an off-heap binary. ... */
typedef struct bin_ref {
    Eterm thing_word;           /* Subtag BIN_REF_SUBTAG. */
    Binary *val;                /* Pointer to Binary structure. */
    struct erl_off_heap_header *next;
} BinRef;
```
The shared object — `erl_binary.h`:
```c
typedef struct binary {
    struct binary_internals intern;   /* flags; apparent_size; erts_refc_t refc; */
    SWord orig_size;
    char orig_bytes[1]; /* to be continued */
} Binary;

ERTS_GLB_INLINE void
erts_bin_release(Binary *bp)
{
    if (erts_refc_dectest(&bp->intern.refc, 0) == 0) {
        erts_bin_free(bp);
    }
}
```
Per-process off-heap list — `erl_message.h`:
```c
typedef struct erl_off_heap {
    struct erl_off_heap_header* first;
    Uint64 overhead;     /* Administrative overhead (used to force GC) */
} ErlOffHeap;
```
Copy increments refc, links node, never touches bytes — `copy.c`:
```c
erts_refc_inc(&(from_br->val)->intern.refc, 2);
to_br->next = off_heap->first;
off_heap->first = (struct erl_off_heap_header*)to_br;
ERTS_BR_OVERHEAD(off_heap, to_br);
```
GC sweep decrements without tracing contents — `erl_gc.c` `sweep_off_heap` (L2996), garbage branch:
```c
        case BIN_REF_SUBTAG:
            erts_bin_release(((BinRef*)ptr)->val);
            break;
        case REF_SUBTAG: ...
            bptr = ((ErtsMRefThing *) ptr)->mb;
            erts_bin_release((Binary *) bptr);
        default:
            ASSERT(is_external_header(ptr->thing_word));
            erts_deref_node_entry(...);
```
vheap accounting in the same sweep: `overhead = (br->val)->orig_size; ... bin_vheap += overhead / sizeof(Eterm); else p->bin_old_vheap += overhead / sizeof(Eterm);` then `p->bin_vheap_sz = next_vheap_size(p, bin_vheap, p->bin_vheap_sz); MSO(p).overhead = bin_vheap;` (L3285-3289). The trigger — `erl_gc.h` L132:
```c
#define ERTS_IS_GC_DESIRED_INTERNAL(Proc, HTop, STop, XtraFlags)	\
    ((((STop) - (HTop) < (Sint)(Proc)->mbuf_sz))                        \
     | ((Proc)->off_heap.overhead > (Proc)->bin_vheap_sz)		\
     | !!((Proc)->flags & (F_FORCE_GC|XtraFlags)))
```
Plus binary pressure forces major GC via the minor precondition (trace 2, L1488). Doc: "The binary heap works as a large object space for binary terms that are greater than 64 bytes... reference counted... a linked list (the MSO - mark and sweep object list)... is woven through the heap"; "The virtual binary heap exists in order to trigger garbage collections earlier when potentially there is a very large amount of off-heap binary data that could be reclaimed."
Links: `<base>/erts/emulator/beam/erl_bits.h`, `erl_binary.h`, `erl_message.h`, `erl_gc.c`, `erl_gc.h`

## 6. Literal areas

Doc: "When garbage collecting a heap (young or old) all literals are left in place and not copied," pseudo-code `if (erts_is_literal(ptr) || (on_old_heap(ptr) && !fullsweep)) { /* do not copy */ }`; implementation of the check: "On 64 bit systems... an area of size 1 GB (by default) is mapped and then all literals are placed within that area. Then all that has to be done to determine if something is a literal or not is two quick pointer checks." (32-bit: 256 KB regions + card-mark bit array; 64-bit Windows: a term tag.)

Purge — `beam_bif_load.c` L1466 protocol comment (excerpt):
> "- The literal area collector process sends copy-literals requests to all processes in the system.
> - Processes inspects their heap for literals in the area, if such are found do a literal-gc to make copies on the heap of all those literals, and then send replies to the literal area collector process."

Entry point `erts_copy_literals_gc` (L1119): `la = ERTS_COPY_LITERAL_AREA(); ... *redsp += erts_garbage_collect_literals(c_p, (Eterm *) literals, lit_bsize, oh, fcalls);`
The literal collector — `erl_gc.c` `erts_garbage_collect_literals` (L1136):
```c
    /*
     * Now the literals are placed in memory that is safe to write into,
     * so now we GC the literals into the old heap.
     */
    ...
    old_htop = sweep_literals_to_old_heap(p->heap, p->htop, old_htop, area, area_sz);   /* L1303 */
    old_htop = sweep_literal_area(p->old_heap, old_htop, ..., area, area_sz);
    ...
    p->old_htop = old_htop;
```
i.e., before an area is freed, every process that references it copies those literals into its own old heap. BIF driver: `erts_internal_purge_module_2` (`beam_bif_load.c` L2088); collector process is `erts_literal_area_collector` (preloaded).
Links: `<base>/erts/emulator/beam/beam_bif_load.c`, `<base>/erts/emulator/beam/erl_gc.c`

## 7. Process termination = wholesale release

`erl_process.c` `delete_process` (L13394), verbatim core:
```c
    /* free all pending messages */
    erts_proc_sig_cleanup_queues(p);
    ...
    /* Clean binaries and funs */
    erts_cleanup_offheap(&p->off_heap);
    erts_cleanup_offheap_list(p->wrt_bins);
    /*
     * The mso list should not be used anymore, but if it is, make sure that
     * we'll notice.
     */
    p->off_heap.first = (void*)(UWord)0x8DEFFACD;
    ...
    /*
     * Release heaps. Clobber contents in DEBUG build.
     */
    erts_deallocate_young_generation(p);
    if (p->old_heap != NULL) {
	ERTS_HEAP_FREE(ERTS_ALC_T_OLD_HEAP, p->old_heap,
		       (p->old_hend-p->old_heap)*sizeof(Eterm));
    }
    erts_erase_dicts(p);
```
`erts_deallocate_young_generation` (`erl_gc.c` L2101) frees the heap block and all fragments via `free_message_buffer(c_p->mbuf)`. The off-heap walk (`erl_message.c` L166) is the *only* per-object work — pure refcount drops:
```c
void erts_cleanup_offheap_list(struct erl_off_heap_header* first)
{
    for (u.hdr = first; u.hdr; u.hdr = u.hdr->next) {
	switch (thing_subtag(u.hdr->thing_word)) {
	case BIN_REF_SUBTAG:  erts_bin_release(u.br->val); break;
	case REF_SUBTAG:      erts_bin_release((Binary *)u.mref->mb); break;
	default:              erts_deref_node_entry(u.ext->node, make_boxed(u.ep));
```
No tracing of the dead heap ever happens. Note also the literal interlock: `if (block_rla_ref) erts_unblock_release_literal_area(block_rla_ref);` — a dying process holds off literal-area frees it might touch.
Link: `<base>/erts/emulator/beam/erl_process.c`

## 8. Funs, exports, NIF resources

Fun entries on this revision are NOT refcounted (change from older OTP; `grep refc erl_fun.c` = zero hits). `erl_fun.h`:
```c
typedef struct erl_fun_entry {
    ErtsDispatchable dispatch;
    ErtsCodePtr pend_purge_address;   /* Address during a pending purge */
    Eterm module;
    byte uniq[16];
    int arity; int index;
    int old_uniq; int old_index;
} ErlFunEntry;

typedef struct erl_fun_thing {        /* on-heap closure */
    Eterm thing_word;                 /* FUN_SUBTAG, arity, env size */
    union { const ErtsDispatchable *disp; const ErlFunEntry *fun; const Export *exp; } entry;
    Eterm env[];                      /* free variables */
} ErlFunThing;
```
Lifetime is handled by the module purge protocol (`erts_fun_purge_prepare/abort_finalize/complete`), not per-reference counting; export entries are likewise permanent-table entries. NIF resources are the refcounted executable-adjacent objects — `erl_nif.c`:
```c
ERL_NIF_TERM enif_make_resource(ErlNifEnv* env, void* obj)
{
    ErtsResource* resource = DATA_TO_RESOURCE(obj);
    ErtsBinary* bin = ERTS_MAGIC_BIN_FROM_UNALIGNED_DATA(resource);
    Eterm* hp = alloc_heap(env, ERTS_MAGIC_REF_THING_SIZE);
    ...
    return erts_mk_magic_ref(&hp, &MSO(env->proc), &bin->binary);   /* linked into off_heap list */
}
void enif_keep_resource(void* obj)    { ... erts_refc_inc(&bin->binary.intern.refc, 2); }
void enif_release_resource(void* obj) { ... erts_bin_release(&bin->binary); }
```
Destructor at refc zero (scheduled, then run):
```c
static int nif_resource_dtor_prologue(Binary* bin)
{   ... erts_schedule_misc_aux_work(sched_id, run_resource_dtor, bin);
    return 0; /* don't free */ }

static void run_resource_dtor(void* vbin)
{   ... if (type->fn.dtor != NULL) { ... type->fn.dtor(&msg_env.env, resource->data); ... } }
```
So a resource is a magic binary riding the exact same off-heap-list + refcount machinery as big binaries, plus a destructor — GC discovers death via the sweep in trace 5 (`REF_SUBTAG` case → `erts_bin_release`).
Links: `<base>/erts/emulator/beam/erl_fun.h`, `<base>/erts/emulator/beam/erl_nif.c`

## 9. Distribution boundary

Encoder — `external.c` L3951-3988 (FUN_DEF case):
```c
                if (is_local_fun(funp)) {
                    const ErlFunEntry *fe = funp->entry.fun;
                    *ep++ = NEW_FUN_EXT;
                    ... /* size patched later */
                    *ep = fun_arity(funp); ep += 1;
                    sys_memcpy(ep, fe->uniq, 16); ep += 16;
                    put_int32(fe->index, ep); ep += 4;
                    put_int32((Uint32)fun_num_free(funp), ep); ep += 4;
                    ep = enc_atom(acmp, fe->module, ep, dflags);
                    ep = enc_term(acmp, make_small(fe->old_index), ...);
                    ep = enc_term(acmp, make_small(fe->old_uniq), ...);
                    ep = enc_pid(acmp, erts_init_process_id, ep, dflags);
                    for (ei = fun_num_free(funp)-1; ei >= 0; ei--)
                        WSTACK_PUSH2(s, ENC_TERM, (UWord) funp->env[ei]);   /* free vars as terms */
                } else {
                    const Export *exp = funp->entry.exp;
                    *ep++ = EXPORT_EXT;
                    ep = enc_atom(acmp, exp->info.mfa.module, ep, dflags);
                    ep = enc_atom(acmp, exp->info.mfa.function, ep, dflags);
                    ep = enc_term(acmp, make_small(exp->info.mfa.arity), ...);
                }
```
No code is serialized — only (module, index, uniq-MD5, free-variable *values*). Spec — `erts/doc/guides/erl_ext_dist.md` "NEW_FUN_EXT": "**`Uniq`** - The 16 bytes MD5 of the significant parts of the Beam file. ... **`Free vars`** - `NumFree` number of terms, each one encoded according to its type." EXPORT_EXT: "This term is the encoding for external funs: `fun M:F/A`." Pids carry node identity — `enc_internal_pid` (L2953):
```c
    *ep++ = NEW_PID_EXT;
    ...
        Eterm sysname = internal_pid_node_name(pid);
        creation = internal_pid_creation(pid);
        ep = enc_atom(acmp, sysname, ep, dflags);
    put_int32(number, ep); ... put_int32(serial, ep); ... put_int32(creation, ep);
```
i.e., pids/refs/funs cross nodes as durable *names* (node + creation epoch, or module + hash), never as pointers or copied code.
Links: `<base>/erts/emulator/beam/external.c`, `<base>/erts/doc/guides/erl_ext_dist.md`

## 10. Costs / discipline

- **Heap fragments** (doc): "Heap fragments contain terms that either did not fit on the heap, or were created by another process and then attached to the heap... All of the heap fragments are considered to be above the high-watermark and part of the young generation." They exist because a sender may not be able to (or want to) touch the receiver's heap; the fragment-vs-heap decision is the trylock dance in trace 4. Cost: extra allocations, deferred GC (`ERTS_IS_GC_DESIRED` includes `STop - HTop < mbuf_sz`).
- **GC-while-message-queue** (doc, on_heap mode): "Using `on_heap` will force all messages to be part of on the young heap which will increase the amount of data that the garbage collector has to move. So if a garbage collection is triggered while processing a large amount of messages, they will be copied to the young heap. This in turn will lead to that the messages will quickly be promoted to the old heap and thus increase its size." (Hence `off_heap` msgq for high-fanin processes; also `move_msgs_to_heap(p)` after minor GC, erl_gc.c L1501.)
- **The invariant** (doc): "any term on the young heap can reference terms on the old heap but *no* term on the old heap may refer to a term on the young heap... If it was, the data would be lost, fire and brimstone would rise to cover the earth. Fortunately, this comes naturally for Erlang because the terms are immutable." Between processes the same holds absolutely: the only inter-heap references permitted are to literal areas and refcounted off-heap objects — both immutable and both excluded from tracing.
- **What it buys:** GC pause bounded by one process's live set (rootset is per-process, trace 3); processes GC independently on any scheduler with zero coordination (no global stop-the-world anywhere in erl_gc.c); termination is O(off-heap list) not O(heap) (trace 7); and copy-on-send makes heaps disposable. The allocator layer beneath (`CarrierMigration.md`: "The ERTS memory allocators manage memory blocks in two types of raw memory chunks... *carriers*") lets whole heap blocks be handed back or migrated between scheduler allocator instances, which is only possible because heaps are self-contained.

Temporary local working copies (not archived) contained `erl_gc.c`, `erl_process.c/h`, `erl_message.c/h`, copy-relevant excerpts, `external.c`, `beam_bif_load.c`, `erl_fun.c`, `GarbageCollection.md`, `CarrierMigration.md`, and `erl_ext_dist.md`, all at sha `d459618c98fedca9d5a1e87327e7e62c24fcd67a`.
