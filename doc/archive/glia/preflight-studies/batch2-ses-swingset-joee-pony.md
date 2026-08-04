All four systems traced against pinned revisions. Raw records follow.

# PINNED REVISIONS

| Repo | Status | Pin |
|---|---|---|
| endojs/endo | active | `c2f412a3172c02676e3e85c1f005508d4328c2e1` (2026-07-29) |
| Agoric/agoric-sdk | active | `00d43ed7b1a91a34de23740b9aa52014f9efc37c` (2026-07-28) |
| davidwagner/joe-e | dormant mirror (last commit 2015-06-10; upstream dev ended ~2013) | `3780dbd749868dc4a4e7c068ff2361e6ee2175ee` |
| ponylang/ponyc | active | `aa29f4c63bfba24521b5d94d98713ecbd6665d4c` (2026-08-02) |

Method note: all quotes were extracted via WebFetch against raw.githubusercontent.com at the pinned shas; the fetch layer summarizes, so quotes are as-extracted (short excerpts verified in-context, but one comment in boyd-gc.js — "variation of behavior" — reads garbled and is flagged below).

---

## SYSTEM 1: SES/Endo

### Record S1 — Compartment construction: per-compartment mutable global over shared frozen intrinsics
- **System:** SES/Endo
- **Repository:** https://github.com/endojs/endo (packages/ses)
- **Repository status:** active
- **Revision or commit:** c2f412a3172c02676e3e85c1f005508d4328c2e1
- **Files and symbols:** `packages/ses/src/compartment.js` (`makeCompartmentConstructor`, privateFields WeakMap), `packages/ses/src/global-object.js` (`setGlobalObjectConstantProperties`, `setGlobalObjectMutableProperties`, `setGlobalObjectEvaluators`)
- **Runtime path traced:** `new Compartment(opts)` → fresh `const globalObject = {}` → constant props → `makeSafeEvaluator({globalObject,...})` → mutable props pointing at shared intrinsics → per-compartment `eval`/`Function` → all per-instance state stored in a module-private WeakMap.
- **Observed implementation:** compartment.js: `const globalObject = {}; ... setGlobalObjectMutableProperties(globalObject, { intrinsics, newGlobalPropertyNames: sharedGlobalPropertyNames, ... });` and `const privateFields = new WeakMap(); weakmapSet(privateFields, compartment, { name, globalTransforms, globalObject, safeEvaluate, resolveHook, importHook, ... moduleMap, moduleMapHook, ... moduleRecords, ... instances, parentCompartment, ... });` global-object.js: `defineProperty(globalObject, name, { value: intrinsics[intrinsicName], writable: true, enumerable: false, configurable: true })` — globals are *writable* aliases to *frozen* intrinsics; constants use `writable: false, configurable: false`.
- **Semantic intent from docs/papers:** README (same sha): "A compartment is an evaluation and execution environment with its own `globalThis` and wholly independent system of modules, but otherwise shares the same batch of intrinsics like `Array`." "By default, Compartments receive no ambient authority."
- **What is verified:** four distinct layers exist in code: (a) shared frozen intrinsics, (b) per-compartment mutable global object whose properties merely *point* at (a), (c) per-compartment module state (`moduleRecords`, `instances`, `deferredExports`), (d) host-endowed authority injected only via constructor options/endowments.
- **What is inferred:** that writable global slots are safe *because* the values behind them are hardened — mutating the binding rebinds the alias, never the shared value.
- **Relevant invariant:** mutability lives only at the compartment edge (binding slots); the shared foundation is value-immutable, so a writable name is not a cross-compartment channel.
- **Consequence for Glia:** directly validates "fresh `Defs` owner per module inheriting a FROZEN shared prelude." The precise SES trick to copy: prelude *names* in a module's `Defs` may be shadowable/rebindable, provided the prelude *values* are deep-frozen; rebinding is then module-local and never a covert channel.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record S2 — Module namespace objects are LIVE VIEWS with a frozen surface
- **System:** SES/Endo
- **Repository:** https://github.com/endojs/endo
- **Repository status:** active
- **Revision or commit:** c2f412a3172c02676e3e85c1f005508d4328c2e1
- **Files and symbols:** `packages/ses/src/module-instance.js` (`liveVar`, `onceVar`, notifiers/updaters, `exportsTarget`)
- **Runtime path traced:** module functor execution → local binding update → `liveVar[localName](newValue)` → updater fan-out → importer's getter reads new value through frozen namespace.
- **Observed implementation:** `defineProperty(exportsTarget, name, { get, set, enumerable: true, configurable: false }); ... freeze(exportsTarget); activate();` — getter: `const get = () => { if (tdz) { throw ReferenceError(\`binding ${q(liveExportName)} not yet initialized\`); } return value; };` — writer side: `const update = freeze(newValue => { value = newValue; tdz = false; for (const updater of updaters) { updater(newValue); } }); liveVar[localName] = update;` — import wiring: `const importNotify = importNotifiers[importName]; for (const updater of updaters) { importNotify(updater); }`
- **Semantic intent from docs/papers:** ESM spec compliance — SES emulates ECMAScript live bindings inside compartments; the namespace object is spec-mandated to be a live, frozen-shaped exotic object.
- **What is verified:** exports are getter-backed live views over mutable closure cells, with TDZ tracking and an updater/notifier graph; the namespace object is `freeze`d yet still delivers changing values ("frozen surface, live interior").
- **What is inferred:** none of this machinery exists for security — it exists to satisfy ESM semantics; the notifier graph is pure cost from the capability standpoint.
- **Relevant invariant:** freezing an object does NOT freeze what its getters report; hardened ≠ constant when accessors are involved.
- **Consequence for Glia:** decisive for the "ordinary map exports vs live binding views" question. Glia's ordinary-map (value snapshot) exports are *simpler and safer*: SES needed an updater graph + TDZ state machine only to honor ESM live bindings, and the result is a confinement subtlety (a "frozen" namespace that mutates observably). If Glia ever adds live views, it must treat every accessor-bearing export as unfrozen for authority analysis.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record S3 — Instance memoization per (compartment, specifier); namespaces shareable across compartments
- **System:** SES/Endo
- **Repository:** https://github.com/endojs/endo
- **Repository status:** active
- **Revision or commit:** c2f412a3172c02676e3e85c1f005508d4328c2e1
- **Files and symbols:** `packages/ses/src/module-link.js` (`link`, `instantiate`, `moduleAliases`, `validateModuleSource`)
- **Runtime path traced:** `compartment.import(specifier)` → `link` → per-compartment `instances` map lookup → recursive link of `resolvedImports`, possibly crossing into `moduleRecord.compartment` of another compartment.
- **Observed implementation:** `const { instances } = weakmapGet(compartmentPrivateFields, compartment); if (mapHas(instances, moduleSpecifier)) { return mapGet(instances, moduleSpecifier); } ... mapSet(instances, moduleSpecifier, moduleInstance);` — cross-compartment: `const importedInstance = link(compartmentPrivateFields, moduleAliases, compartment, resolvedSpecifier);` — validation: `isArray(imports) || Fail\`Invalid module source: 'imports' must be an array\`;`
- **Semantic intent from docs/papers:** README: module descriptors supplied via "the `modules` map," "`moduleMapHook`," or "`importHook`/`importNowHook`" — the module map is how one compartment deliberately grants another a namespace.
- **What is verified:** SES instantiates a module ONCE per compartment (memoized per (compartment, specifier)); all importers within a compartment share one instance and thus share its private mutable state; a namespace can be aliased into another compartment as an explicit grant.
- **What is inferred:** shared-instance identity is load-bearing for the ecosystem (registries, caches, `instanceof` across importers).
- **Relevant invariant:** module instance = unit of shared state and identity; the compartment boundary, not the import edge, is the isolation quantum.
- **Consequence for Glia:** CHALLENGES the "per-import module instantiation" proposal. Every system inspected memoizes instances per container; per-import instantiation gives stronger confinement but forfeits shared module identity (two importers get distinct closures, distinct private state, non-`eq` exports). Glia must decide deliberately: per-import = each import is its own compartment-equivalent. Needs an identity-semantics decision before PR-1-adjacent module work; a spike demonstrating both behaviors on a stateful module is the cheap way to force the choice.
- **Confidence:** High
- **Classification:** CHALLENGES CURRENT DESIGN

### Record S4 — lockdown/harden: transitive freeze + early capture as the precondition for sharing
- **System:** SES/Endo
- **Repository:** https://github.com/endojs/endo
- **Repository status:** active
- **Revision or commit:** c2f412a3172c02676e3e85c1f005508d4328c2e1
- **Files and symbols:** `packages/ses/src/lockdown.js` (`hardenIntrinsics`, `tamedHarden`), `packages/ses/src/make-hardener.js` (`toFreeze` set, `hardened` WeakSet, `freezeTypedArray`), `packages/ses/src/commons.js` (early intrinsic capture, `uncurryThis`)
- **Runtime path traced:** `lockdown()` → repair/remove unpermitted intrinsics → `hardenIntrinsics()` → `tamedHarden(toHarden)` → transitive freeze walk; `harden` also installed as an intrinsic for user code.
- **Observed implementation:** make-hardener.js: `const toFreeze = new Set(); function enqueue(val) { if (isPrimitive(val)) return; if (weaksetHas(hardened, val) || setHas(toFreeze, val)) return; setAdd(toFreeze, val); }` with own-descriptor + prototype traversal, and the comment "get stable/immutable outbound links before a Proxy has a chance to do something sneaky"; typed arrays: `if (isTypedArray(obj)) { freezeTypedArray(obj); } else { freeze(obj); }` (index properties stay writable per spec, so only non-index props are downgraded). commons.js top comment: "Captures native intrinsics during initialization, so vetted shims ... are free to modify the environment without compromising the integrity of SES", with `export const arrayMap = uncurryThis(arrayPrototype.map);` etc. lockdown.js: `const tamedHarden = tameHarden(safeHarden, __hardenTaming__); addIntrinsics({ harden: tamedHarden });`
- **Semantic intent from docs/papers:** README: "`lockdown()` tamper-proofs all of the JavaScript intrinsics, to prevent prototype pollution"; "`harden` ... ensures that every object in the transitive closure over property and prototype access ... has been frozen."
- **What is verified:** hardening is a worklist transitive freeze with a WeakSet membership ledger; hardening runs BEFORE any compartment shares the intrinsics; the runtime's own primitives are captured before adversarial code can run.
- **What is inferred:** the WeakSet `hardened` ledger is the operational form of a "DeepFrozen" predicate — an O(1) certified-immutable check after one O(n) certification pass.
- **Relevant invariant:** share-only-after-harden; and "is hardened" is a property you record once, not re-verify per use.
- **Consequence for Glia:** direct blueprint for Glia's DeepFrozen/authority-free checks: implement a hardened-ledger (a `HashSet<ValId>`-style membership set or a bit on the value) written by one transitive certification pass over the prelude before any module instantiates. Rust removes the Proxy/prototype-poisoning threat, but the ordering rule (freeze prelude before first module `Defs` is born) and the O(1) ledger transfer intact.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

---

## SYSTEM 2: Agoric SwingSet (liveslots)

### Record A1 — The four-way table split: strong for escaped exports, weak for everything else
- **System:** Agoric SwingSet
- **Repository:** https://github.com/Agoric/agoric-sdk
- **Repository status:** active
- **Revision or commit:** 00d43ed7b1a91a34de23740b9aa52014f9efc37c
- **Files and symbols:** `packages/swingset-liveslots/src/liveslots.js` (`valToSlot`, `slotToVal`, `exportedRemotables`, `vreffedObjectRegistry`, `convertValToSlot`, `convertSlotToVal`, `finalizeDroppedObject`)
- **Runtime path traced:** value crosses vat boundary → `convertValToSlot` allocates `o+NN` and registers `valToSlot.set(val, slot); slotToVal.set(baseRef, new WeakRef(val))`; inbound `o-NN` → `convertSlotToVal` → `makeImportedPresence(slot, iface)` → `registerValue`; drop of last in-vat ref → FinalizationRegistry fires.
- **Observed implementation:** `const valToSlot = new WeakMap(); // object -> vref` / `const slotToVal = new Map(); // baseRef -> WeakRef(object)` / `const exportedRemotables = new Set(); // objects` with comment "We use two weak maps plus the strong `exportedRemotables` set, because it seems simpler than using four separate maps (import-vs-export times strong-vs-weak)." Finalizer: `function finalizeDroppedObject(baseRef) { const wr = slotToVal.get(baseRef); if (wr && !wr.deref()) { addToPossiblyDeadSet(baseRef); slotToVal.delete(baseRef); } }` File comment: "We retain a weak reference to the Presence, and use a FinalizationRegistry to learn when the vat has dropped it, so we can notify the kernel."
- **Semantic intent from docs/papers:** `packages/SwingSet/docs/garbage-collection.md` (same sha) — vats must tell the kernel when imports are dropped; kernel mirrors with c-lists.
- **What is verified:** an exported object escaping the vat is pinned by an explicit STRONG set (`exportedRemotables`) — i.e., "escaped ⇒ strong" is enforced by a manual write barrier at the marshalling boundary, while the translation tables themselves are weak.
- **What is inferred:** the strong set exists because the kernel may deliver a message to the export at any time; liveness obligation follows the *grant*, not local reachability.
- **Relevant invariant:** weak-at-rest, strong-when-escaped — exactly, and the flip happens at the serialization boundary, in one place.
- **Consequence for Glia:** strongest possible confirmation of the closures-carry-weak/strong-owner-refs design: a production capability runtime independently converged on the same barrier ("strong on escape, weak otherwise"), and centralized the flip at the single choke point where values cross the boundary. Glia should likewise confine the weak→strong promotion to one marshalling/escape function, never scattered call sites.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record A2 — reachable vs recognizable, drop vs retire: authority, liveness, and identity are three ledgers
- **System:** Agoric SwingSet
- **Repository:** https://github.com/Agoric/agoric-sdk
- **Repository status:** active
- **Revision or commit:** 00d43ed7b1a91a34de23740b9aa52014f9efc37c
- **Files and symbols:** `packages/SwingSet/docs/garbage-collection.md`; `packages/swingset-liveslots/src/liveslots.js` (`dropExports`, `retireOneExport`, `retireImports`)
- **Runtime path traced:** kernel loses last strong ref to a vat's export → `dispatch.dropExports` → `exportedRemotables.delete(o)` and `vrm.setExportStatus(vref, 'recognizable')`; later, when identity too is gone → `retireExports` → `setExportStatus(vref, 'none')`; importer-side weak-only recognition ends via `retireImports` → `vrm.ceaseRecognition(vref)`.
- **Observed implementation:** `function dropExports(vrefs) { ... exportedRemotables.delete(o); ... if (virtual || durable) { vrm.setExportStatus(vref, 'recognizable'); } }`; `function retireOneExport(vref) { ... vrm.setExportStatus(vref, 'none'); ... kernelRecognizableRemotables.delete(vref); }`; `function retireImports(vrefs) { for (const vref of vrefs) { vrm.ceaseRecognition(vref); } }`
- **Semantic intent from docs/papers:** GC doc: reachability = can produce/message the object; recognizability = can identify it (WeakMap key) without keeping it alive; "The `recognizable` count will always be equal or greater than the `reachable` count"; `dropImports` when "a Presence is collected and the VOM disavows reachability", `retireImports` "when the VOM disavows recognizability as well."
- **What is verified:** the protocol has two distinct downgrade transitions (drop = lose authority/liveness obligation; retire = lose identity) and the kernel keeps two counters per object.
- **What is inferred:** the split exists because weak collections create observers that hold no authority — merging the ledgers either leaks (treat recognizers as keepers) or breaks weak-key semantics (retire too early).
- **Relevant invariant:** authority (can invoke) ⊃ liveness obligation (must keep) ⊃ identity (can still be recognized) — three separately-tracked states with monotonic downgrades.
- **Consequence for Glia:** answers the KEY QUESTION "how is kept-alive separated from possesses-authority" concretely: by *state on the reference edge*, not by GC. If Glia modules can be dropped while other modules still hold names/keys referring to their exports, Glia needs at least a two-state edge (reachable vs recognizable) even with deterministic Rc drops — Rc handles liveness but not the "identity outlives authority" case (weak-keyed tables, interned symbols naming dead exports). Spike: model drop/retire on Glia's `Defs` edges before committing the module lifecycle API.
- **Confidence:** High
- **Classification:** REQUIRES A SPIKE

### Record A3 — bringOutYourDead: GC observation is quarantined into an explicit, deterministic phase
- **System:** Agoric SwingSet
- **Repository:** https://github.com/Agoric/agoric-sdk
- **Repository status:** active
- **Revision or commit:** 00d43ed7b1a91a34de23740b9aa52014f9efc37c
- **Files and symbols:** `packages/swingset-liveslots/src/boyd-gc.js` (`scanForDeadObjects`, pillars, `possiblyDeadSet`), `liveslots.js` (`makeBOYDKit`)
- **Runtime path traced:** kernel sends `bringOutYourDead` → `gcTools.gcAndFinalize()` → loop over `[...possiblyDeadSet].sort()` → per-vref check (`checkExportRepresentative` / `checkExportRemotable` / `checkImportPresence`) → accumulate sets → single sorted syscall batch.
- **Observed implementation:** doc-comment: "A logical object is either VREF-REACHABLE, VREF-RECOGNIZABLE, or nothing", with "pillars" holding it up — Presences: "RAM pillar + vdata pillar"; Remotables: "RAM pillar only"; invariant "you only add a vref to possiblyDeadSet if it was VREF-REACHABLE first". Loop: `do { gcAgain = false; await gcTools.gcAndFinalize(); for (const vrefOrBaseRef of [...possiblyDeadSet].sort()) { ... if (res.dropImport) importsToDrop.add(res.dropImport); ... gcAgain ||= !!res.gcAgain; } } while (possiblyDeadSet.size > 0 || gcAgain);` then `if (importsToDrop.size) syscall.dropImports([...importsToDrop].sort());` etc. (One extracted phrase about sorting "variation of behavior" came through garbled; the verified mechanism is: sorted iteration + sorted syscalls for determinism.)
- **Semantic intent from docs/papers:** GC doc: GC effects must be deterministic/consensus-safe, so organic engine GC is never allowed to directly cause observable effects.
- **What is verified:** liveslots re-derives deadness inside the explicit phase (re-`deref`, pillar checks, fixpoint loop) rather than trusting finalizer timing; all lifecycle syscalls are emitted sorted, in one batch, at a scheduled point.
- **What is inferred:** the "pillar" enumeration is the debugging-forced explicit model of "what keeps this alive" — RAM refs, virtualized data, and export status each counted separately.
- **Relevant invariant:** finalization signals are hints; truth is recomputed transactionally at an explicit phase boundary, to a fixpoint.
- **Consequence for Glia:** even with deterministic `Rc` drops, Glia should not let `Drop` impls perform authority-visible actions directly (drop order inside a scope, RefCell reentrancy during drop, and cycle-leaked Rcs all make Drop-time effects unreliable). Transferable pattern: Drop only enqueues into a `possiblyDeadSet`; an explicit `collect()` phase (end of eval turn) re-verifies and performs deregistration in sorted order. This is the lifecycle machinery that "became necessary beyond ordinary GC" — and it is needed beyond ordinary Rc too.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record A4 — Virtual/durable objects: authority and state persist independently of the RAM representative
- **System:** Agoric SwingSet
- **Repository:** https://github.com/Agoric/agoric-sdk
- **Repository status:** active
- **Revision or commit:** 00d43ed7b1a91a34de23740b9aa52014f9efc37c
- **Files and symbols:** `packages/swingset-liveslots/src/virtualObjectManager.js` (`makeRepresentative`, `reanimateVO`, `contextCache`, `dataCache`, `defineDurableKind`), `virtualReferences.js` via `vrm` (`addReachableVref`, `updateReferenceCounts`)
- **Runtime path traced:** method call on representative → `contextProvider(this)` → `valToSlot` → baseRef → `contextCache`/`dataCache` → `syscall.vatstoreGet/Set`; state writes: `vrm.updateReferenceCounts(oldSlots, newSlots)`.
- **Observed implementation:** "All virtual-object state is keyed by baseRef, like o+v11/5." "A representative is the manifestation of a virtual object that vat code has direct access to. A given virtual object can have at most one representative, which will be created as needed." State-slot refcounting: `valueCD.slots.forEach(vrm.addReachableVref); ... vrm.updateReferenceCounts(oldSlots, newSlots);` Durable kinds: "the stateShape is serialized and recorded in the durableKindDescriptor, so future incarnations ... can both check for compatibility, and ... decrement refcounts."
- **Semantic intent from docs/papers:** virtual = pageable out of RAM within an incarnation; durable = survives vat upgrade.
- **What is verified:** the in-RAM object is a cache entry (at most one, re-creatable); the object's authority-bearing identity and state live in the vatstore with explicit refcounts; references *from disk state* count as reachability (the "vdata pillar").
- **What is inferred:** this is the endpoint of the kept-alive/authority separation: an object can possess and confer authority while having zero live RAM footprint.
- **Relevant invariant:** identity and authority are ledger entries; memory objects are reconstructible views keyed by that ledger.
- **Consequence for Glia:** FUTURE TRACK for persistence/upgrade (relevant to the child-bootstrap immutable initial-authority record: authority as durable ledger, representative as cache). Not needed for PR-1 module ownership, but the design constraint to preserve now: nothing in Glia's module/`Defs` design should assume "authority ⇒ live Rust object," or virtualization later becomes a rewrite.
- **Confidence:** High
- **Classification:** FUTURE TRACK

---

## SYSTEM 3: Joe-E

### Record J1 — Verifier: ordinary Java features rejected as ambient authority / confinement hazards
- **System:** Joe-E
- **Repository:** https://github.com/davidwagner/joe-e
- **Repository status:** dormant mirror (last commit 2015-06-10; canonical dev ended ~2013 on Google Code)
- **Revision or commit:** 3780dbd749868dc4a4e7c068ff2361e6ee2175ee
- **Files and symbols:** `eclipse/src/org/joe_e/eclipse/Verifier.java` (~93 KB), `Taming.java`
- **Runtime path traced:** static verification pass at compile time (Eclipse builder), not runtime: per-field, per-method AST checks emitting problems.
- **Observed implementation:** static fields: `if (Modifier.isFinal(modifiers)) { if (!taming.implementsOverlay(fieldTB, taming.POWERLESS)) { addProblem("Non-powerless static field " + name, fb); } } else { addProblem("Non-final static field " + name, fb); }` — native: `addProblem("Native method " + name, name);` — finalizers: `if (name.getIdentifier().equals("finalize") && md.parameters().isEmpty()) { addProblem("Finalizers are not allowed", name); }` — untamed types: `addProblem("Reference to disabled type " + itb.getName(), sn, itb);` — exceptions: `addProblem("Catching type Throwable is not allowed", ...); ... addProblem("Catching an Error is not allowed", ...)` — plus a ban on constructing anonymous classes during enclosing-object initialization.
- **Semantic intent from docs/papers:** README (in-repo): Joe-E "restricts Java code to guarantee additional security properties but does not modify programs or change their meaning." (Mettler/Wagner/Close, "Joe-E: A Security-Oriented Subset of Java," NDSS 2010 — external, used as intent only.)
- **What is verified:** the concrete hazard list, with exact error strings: mutable static state (= ambient authority reachable from any code without a grant), native methods (= unauditable host authority), finalizers (= code runs with no caller/attacker-controlled timing, resurrects "dead" authority), untamed library types, catching Error (= observing/suppressing VM-level failures to keep running in a corrupted state).
- **What is inferred:** the constructor/anonymous-class rule targets the classic "this-escape during construction" leak — partially-initialized objects escaping as capabilities.
- **Relevant invariant:** every name reachable without an explicit grant must be authority-free; anything that runs code at non-programmatic times (finalizers) or outside the language (native) breaks capability reasoning.
- **Consequence for Glia:** Glia's checklist of "seemingly-ordinary features that become hazards": (1) any mutable slot in the shared prelude/`Defs` visible to all modules = a Joe-E static field — the frozen prelude is exactly the right fix, and the *lint* is "non-frozen value in shared scope"; (2) Rust `Drop` impls on capability-bearing values = finalizers — must not exercise authority (converges with Record A3); (3) host-native builtins = native methods — admissible only via the taming/grant path; (4) Glia's error/condition handling must not let modules intercept runtime-integrity failures of other modules.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record J2 — Immutable/Powerless overlay types: "no mutable state" ≠ "no authority"
- **System:** Joe-E
- **Repository:** https://github.com/davidwagner/joe-e
- **Repository status:** dormant mirror
- **Revision or commit:** 3780dbd749868dc4a4e7c068ff2361e6ee2175ee
- **Files and symbols:** `library/src/org/joe_e/Immutable.java`, `library/src/org/joe_e/Powerless.java` (and `Token` referenced therein)
- **Runtime path traced:** none (marker interfaces checked by the verifier's overlay type system).
- **Observed implementation:** Immutable.java, in full: `/** Marker interface for annotating classes that transitively do not contain any mutable state. Joe-E requires that classes that implement this interface meet the obligation that all fields must be (1) final and (2) of a declared type that implements this interface in the overlay type system. */ public interface Immutable { }` Powerless: "Marker interface for annotating classes that transitively do not contain any mutable state **or tokens**" — must not extend `Token`, all fields final and Powerless.
- **Semantic intent from docs/papers:** Powerless ⊂ Immutable: an immutable object can still *wield* authority (hold a capability token); Powerless additionally certifies it confers none.
- **What is verified:** two distinct transitive predicates exist in the library, and only the stronger one (Powerless) is accepted for statics (see J1 quote: "Non-powerless static field").
- **What is inferred:** the split exists precisely because freezing does not neutralize capabilities — an immutable record containing a file handle is frozen and still dangerous.
- **Relevant invariant:** deep-frozen and authority-free are different transitive closures; shared/ambient positions require the intersection.
- **Consequence for Glia:** Glia's "authority-free checks/DeepFrozen" must be TWO predicates, not one: `DeepFrozen` (no mutation channel) and `Powerless`/authority-free (no capability values in the transitive closure). The shared prelude must be certified for *both*; a frozen closure capturing a capability may be shareable-as-granted but never prelude-eligible. This maps cleanly onto the tagged-data Cell work in f1365b6: authority-bearing tags are what the Powerless walk rejects.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record J3 — Taming database: default-deny of host static authority, member by member
- **System:** Joe-E
- **Repository:** https://github.com/davidwagner/joe-e
- **Repository status:** dormant mirror
- **Revision or commit:** 3780dbd749868dc4a4e7c068ff2361e6ee2175ee
- **Files and symbols:** `safej/java/lang/System.safej` (plus 122 sibling `.safej` files in `safej/java/lang/`; `apps/servlet/src/org/joe_e/taming/Policy.java`, 101 KB generated policy)
- **Runtime path traced:** verifier consults taming DB per referenced member; untamed member reference = compile error (see J1).
- **Observed implementation:** System.safej allows exactly one static method — `arraycopy(Object, int, Object, int, int)`. Denied with "default deny": `err`, `in`, `out`, `exit(int)`, `getProperty(String)`, `currentTimeMillis()`, `load(String)`, `loadLibrary(String)`, `setSecurityManager(...)`; `console()` annotated "gives unmediated access to the console".
- **Semantic intent from docs/papers:** taming = manually auditing a host API surface and admitting only authority-free members into the ambient namespace; everything with authority must arrive as an explicit capability instead.
- **What is verified:** the default-deny posture is literal file content, and even `currentTimeMillis()` (clock read) is treated as authority.
- **What is inferred:** the 101 KB generated Policy.java shows taming is a large, ongoing curation cost — the file header itself admits incomplete auditing.
- **Relevant invariant:** the host boundary is a per-member allowlist; "harmless-looking" reads (clock, env) are authority because they are unforgeable ambient inputs.
- **Consequence for Glia:** Glia's Rust-builtin surface (and std/caps, std/kernel endowments) needs the same shape: a per-builtin classification into prelude-eligible (Powerless: `cons`, arithmetic, `str/upcase`) vs grant-only (clock, random, IO, spawn) — consistent with the approved constructive `create_child(image, grants)` design. Budget for the curation cost: the taming DB was the largest artifact in Joe-E, and the reverted catalog/lint (#630/#631) will eventually be needed in some form to keep this surface audited.
- **Confidence:** High (file contents verified; Policy.java size from tree listing)
- **Classification:** CONFIRMS CURRENT DESIGN

---

## SYSTEM 4: Pony (contrast case)

### Record P1 — ORCA runtime: local tracing + cross-actor message-based deferred RC, barrier at send time
- **System:** Pony
- **Repository:** https://github.com/ponylang/ponyc
- **Repository status:** active
- **Revision or commit:** aa29f4c63bfba24521b5d94d98713ecbd6665d4c
- **Files and symbols:** `src/libponyrt/gc/gc.c` (`ponyint_gc_sendobject/recvobject`, `ponyint_gc_acquire/release`, `mark_remote_object`, `ponyint_gc_markimmutable`), `gc.h` (`gc_t { uint32_t mark; ... size_t rc; objectmap_t local; actormap_t foreign; deltamap_t* delta; }`), `objectmap.c`/`actormap.c`
- **Runtime path traced:** actor sends message containing object → `gc_sendobject` adjusts per-recipient rc (`if(aref->rc <= 1) { aref->rc += (GC_INC_MORE - 1); acquire_object(ctx, actor, p, true); } else { aref->rc--; }`); receiver `recv_remote_object`: `obj->rc++; obj->mark = gc->mark;`; local collection: `if(!ponyint_heap_mark(chunk, p)) recurse(ctx, p, t->trace);`; foreign bookkeeping: `actorref_t* aref = ponyint_actormap_getorput(&gc->foreign, actor, gc->mark);`
- **Semantic intent from docs/papers:** ORCA ("Orca: GC and Type System Co-Design for Actor Languages," Clebsch et al., OOPSLA 2017 — external; in-repo gc.h has no protocol comment): message-based deferred reference counting made sound BY the type system (no data races to count).
- **What is verified:** per-actor heaps with mark tracing locally; cross-actor lifetime via rc deltas piggybacked on messages plus explicit ACQUIRE/RELEASE batching (`GC_INC_MORE`); every actor keeps an `objectmap` (its objects others see) and `actormap` (foreign objects it sees).
- **What is inferred:** correctness depends on the static capability system — the runtime never scans other heaps, it *trusts the types* that no unsynchronized aliases exist.
- **Relevant invariant:** all lifetime bookkeeping happens at the message-send boundary — the one place references change owners.
- **Consequence for Glia:** the GC scheme itself does not transfer (multi-actor, racy-free counting vs single-threaded Rc). The one transferable nugget: bookkeeping is performed by the SENDER at the escape point — the same single-choke-point discipline as liveslots' `convertValToSlot` and Glia's proposed manual write barrier at closure escape. Three systems now agree: put the barrier where the reference crosses an ownership boundary, nowhere else.
- **Confidence:** High for code; Medium for protocol naming (paper external)
- **Classification:** NOT TRANSFERABLE — GC DIFFERENCE

### Record P2 — Reference capabilities: ownership state on the REFERENCE, viewpoint adaptation, and `tag` as static "recognizable-without-authority"
- **System:** Pony
- **Repository:** https://github.com/ponylang/ponyc
- **Repository status:** active
- **Revision or commit:** aa29f4c63bfba24521b5d94d98713ecbd6665d4c
- **Files and symbols:** `src/libponyc/type/cap.c` (`is_cap_sub_cap`, `cap_view_upper`, `cap_view_lower`, `cap_single`/`cap_aliasing`, `cap_sendable`), `src/libponyc/type/viewpoint.c` (`viewpoint_type`, `viewpoint_upper/lower`)
- **Runtime path traced:** compile-time only: field access through a receiver → `cap_view_upper(left, right)`; alias creation → `cap_aliasing`; message send → `cap_sendable`.
- **Observed implementation:** subtyping: `case TK_ISO: return (super == TK_ISO);` (iso invariant), trn/ref each `<: box`, `case TK_VAL: ... TK_BOX: TK_CAP_SHARE: return true;`. Viewpoint under iso: `case TK_ISO: { switch(*right_cap) { case TK_ISO: case TK_CAP_SEND: if(left_eph == TK_EPHEMERAL) *right_eph = TK_EPHEMERAL; break; case TK_VAL: case TK_CAP_SHARE: break; default: *right_cap = TK_TAG; *right_eph = TK_NONE; } }` — i.e., mutable state seen through an iso collapses to `tag`. Under box: `case TK_ISO: *right_cap = TK_TAG;`. Sendability: `bool cap_sendable(token_id cap) { switch(cap) { case TK_ISO: case TK_VAL: case TK_TAG: ... return true; } return false; }`
- **Semantic intent from docs/papers:** "Deny Capabilities for Safe, Fast Actors" (Clebsch et al., AGERE 2015 — external; in-repo cap.c carries no deny commentary): each capability is defined by what it DENIES to other aliases; `tag` denies both read and write to the holder — identity/send-target only.
- **What is verified:** the adaptation table is real code: strength is a property of the reference-path, recomputed at each dereference (origin ⊳ field), and only `{iso, val, tag}` cross actor boundaries.
- **What is inferred:** `tag` is the static twin of SwingSet's "recognizable": you may compare identity and address the object, but can neither read nor mutate — recognition without authority, enforced at compile time instead of by two kernel counters.
- **Relevant invariant:** what you can do through a reference = f(how you hold it, not just what it is); attenuation is automatic at every dereference.
- **Consequence for Glia:** almost all of this is the type system doing statically what Glia does dynamically — not portable into an Rc/RefCell Lisp without becoming a different language. Two transferable concepts only: (1) ownership STATES on references (Glia's weak/strong positional strength is a 2-point dynamic version of Pony's 6-point static lattice; naming the states and their legal transitions, Pony-style, will keep the write-barrier rules coherent); (2) a `tag`-like third state — "identity-only reference" — is worth reserving in Glia's design space, since A2 shows capability runtimes need it eventually (weak tables, registries) and Pony shows it can be a first-class reference state rather than a bolt-on.
- **Confidence:** High for code; Medium for deny-semantics framing (paper external)
- **Classification:** NOT TRANSFERABLE — STATIC TYPE-SYSTEM DIFFERENCE

---

# OCAP MATRIX

| Field | SES/Endo | Agoric SwingSet (liveslots) | Joe-E | Pony (CONTRAST) |
|---|---|---|---|---|
| Authority source | endowments passed into Compartment; intrinsics stripped of authority at lockdown | c-list entries (kernel-mediated object refs); device nodes at kernel edge | constructor-passed references; taming DB default-denies host statics | any non-tag reference; actors as addressable capabilities |
| Lexical capture carries authority? | yes — closures over endowments are the grant mechanism | yes — Presences/Remotables captured in closures | yes — fields/locals; statics banned so *only* capture carries it | yes, but capability of the *reference* bounds use; capture adapts via viewpoint |
| Module/compartment owner | Compartment instance (privateFields WeakMap: globalObject, moduleMap, instances) | vat (one liveslots instance = one heap + tables) | none at runtime — class = verified unit; no instance-level container | actor (own heap, own gc_t) |
| Shared foundation | frozen intrinsics shared by all compartments | frozen intrinsics per vat (SES inside vat) + kernel | tamed JDK subset (allowlisted members) | runtime + `val` (globally shareable immutable) |
| Mutability hardening | `harden` transitive freeze + `hardened` WeakSet ledger; lockdown-before-share | harden at vat boundaries (marshalled data immutable-by-copy) | static verification: Immutable/Powerless transitive final-field proof | `val`/`box` deny-write, compile-time; `obj->immutable` runtime bit |
| Export form | frozen module namespace with LIVE getter bindings (updater graph) | `o+NN` slots in c-lists; namespace irrelevant — object refs only | public methods of verified classes | behaviours (async) + sendable refs `{iso,val,tag}` |
| Capability-method representation | ordinary JS methods on hardened objects | eventual-send to Presence → kernel run-queue → dispatch.deliver | ordinary Java methods | behaviours (async, no return) vs functions (sync, capability-checked receiver) |
| Attenuation/wrapping | manual facet objects; compartment global as attenuated world-view | manual facets; virtual-object multi-facet kinds built in | manual wrappers; taming as edge attenuation | automatic: viewpoint adaptation attenuates at every field access |
| Reentrancy model | synchronous plan-interference risk; hardening mitigates state, not sequencing | none within a turn — strict turn-based delivery per vat (run-to-completion) | synchronous Java; verifier does not solve reentrancy (constructor-escape ban only) | none — behaviours serialize per actor |
| Lifetime/GC | JS engine GC; WeakSet ledger for hardened; per-compartment instance maps | engine GC + WeakRef/FinalizationRegistry + possiblyDeadSet + BOYD fixpoint + drop/retire/abandon protocol + vatstore refcounts | JVM GC untouched (finalizers banned so GC is authority-silent) | per-actor mark tracing + ORCA message-counted cross-actor refs |
| Host trust boundary | importHook/endowments; vetted shims before lockdown | kernel + device vats; syscalls only | taming database (.safej per member, default deny) | FFI/native trusted; type system trusted by runtime |
| Kept-alive vs authority distinction | not separated (GC invisible; hardened ≠ authority-free is the analogous split) | fully explicit: REACHABLE vs RECOGNIZABLE vs none; strong `exportedRemotables` for escaped; authority can live on disk with zero RAM presence | orthogonal by construction: liveness never grants (no statics), death never acts (no finalizers) | statically: `tag` = alive-addressable-unreadable; rc ≠ rights (types carry rights) |
| Transferable lesson | freeze the shared floor first, keep one hardened-ledger, memoize instances per container (decide identity semantics!) | put the weak→strong flip at one escape choke point; quarantine death-observation into an explicit collect phase; track drop (authority) and retire (identity) separately | shared-scope values need Powerless (authority-free), not just frozen; default-deny the host surface; ban authority in destructors | name your reference-strength states and their transitions; reserve an identity-only reference state |

# PINNED LINKS

- https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/compartment.js · [module-instance.js](https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/module-instance.js) · [module-link.js](https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/module-link.js) · [global-object.js](https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/global-object.js) · [lockdown.js](https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/lockdown.js) · [make-hardener.js](https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/make-hardener.js) · [commons.js](https://github.com/endojs/endo/blob/c2f412a3172c02676e3e85c1f005508d4328c2e1/packages/ses/src/commons.js)
- https://github.com/Agoric/agoric-sdk/blob/00d43ed7b1a91a34de23740b9aa52014f9efc37c/packages/swingset-liveslots/src/liveslots.js · [boyd-gc.js](https://github.com/Agoric/agoric-sdk/blob/00d43ed7b1a91a34de23740b9aa52014f9efc37c/packages/swingset-liveslots/src/boyd-gc.js) · [virtualObjectManager.js](https://github.com/Agoric/agoric-sdk/blob/00d43ed7b1a91a34de23740b9aa52014f9efc37c/packages/swingset-liveslots/src/virtualObjectManager.js) · [garbage-collection.md](https://github.com/Agoric/agoric-sdk/blob/00d43ed7b1a91a34de23740b9aa52014f9efc37c/packages/SwingSet/docs/garbage-collection.md)
- https://github.com/davidwagner/joe-e/blob/3780dbd749868dc4a4e7c068ff2361e6ee2175ee/eclipse/src/org/joe_e/eclipse/Verifier.java · [Immutable.java](https://github.com/davidwagner/joe-e/blob/3780dbd749868dc4a4e7c068ff2361e6ee2175ee/library/src/org/joe_e/Immutable.java) · [Powerless.java](https://github.com/davidwagner/joe-e/blob/3780dbd749868dc4a4e7c068ff2361e6ee2175ee/library/src/org/joe_e/Powerless.java) · [System.safej](https://github.com/davidwagner/joe-e/blob/3780dbd749868dc4a4e7c068ff2361e6ee2175ee/safej/java/lang/System.safej)
- https://github.com/ponylang/ponyc/blob/aa29f4c63bfba24521b5d94d98713ecbd6665d4c/src/libponyrt/gc/gc.c · [gc.h](https://github.com/ponylang/ponyc/blob/aa29f4c63bfba24521b5d94d98713ecbd6665d4c/src/libponyrt/gc/gc.h) · [cap.c](https://github.com/ponylang/ponyc/blob/aa29f4c63bfba24521b5d94d98713ecbd6665d4c/src/libponyc/type/cap.c) · [viewpoint.c](https://github.com/ponylang/ponyc/blob/aa29f4c63bfba24521b5d94d98713ecbd6665d4c/src/libponyc/type/viewpoint.c)

Headline deltas for the design decision: one CHALLENGES (S3 — every inspected system memoizes module instances per container; per-import instantiation forfeits shared identity and needs an explicit decision), one REQUIRES A SPIKE (A2 — drop-vs-retire shows Rc alone cannot express "identity outlives authority"; model it on `Defs` edges), and a three-system convergence (A1, P1, S4) that the weak→strong/harden barrier belongs at exactly one escape choke point.