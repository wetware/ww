All source tracing is complete. Compiling the final records.

---

# SOURCE-FIRST STUDY: E-on-Java, Monte/Typhon, Newspeak — module/definition-ownership runtime design for Glia

**Pinned revisions (via `api.github.com/repos/<org>/<repo>/commits?per_page=1`, fetched 2026-08-03):**
- kpreid/e-on-java @ `a0b3b599cf267b3138eea5f5fb83f27cebd28373` (2011-10-26; dormant mirror)
- monte-language/typhon @ `92d70fbcbe1291f1aa7c5cedca90345b8a95f6cc` (2025-10-22; active)
- monte-language/monte (docs only) @ `07fb0d6985454010f71a387613cec901876c545a` (2020-12-24)
- newspeaklanguage/newspeak @ `945b81e80d5940ccca1780144c58dc416666ed86` (2026-07-24; active)
- newspeaklanguage/primordialsoup @ `9ac43beaca8b006dce62ccd237dfb2f3f3c833c5` (2024-08-24)

**Fetch failures disclosed:** erights.org unreachable (ECONNREFUSED, both `www.` and bare host; web.archive.org blocked by tooling) — E semantic intent is taken from doc comments inside the pinned repo instead. `typhon/env.py` does not exist at the pinned revision (scope analysis lives in `typhon/nano/scopes.py`). Two Newspeak root-listing entries reported by the fetch summarizer (`NSCompiler.ns`, `OO_AND_CAPABILITIES.md`) returned 404 on direct fetch — treated as summarizer hallucination; nothing below depends on them. All quotes below were fetched through WebFetch's summarizer model against raw pinned URLs; whole-file quotes (makeCaretaker, BankAccount) are highest-fidelity.

---

## SYSTEM 1: E-ON-JAVA

### Record E-1 — Object construction: closure = auditor approvals + pruned field array + shared outers + method table

- **System:** E-on-Java
- **Repository:** https://github.com/kpreid/e-on-java
- **Repository status:** Dormant mirror (last commit 2011-10-26); canonical E implementation
- **Revision or commit:** `a0b3b599cf267b3138eea5f5fb83f27cebd28373`
- **Files and symbols:** `src/jsrc/org/erights/e/elang/evm/ObjectExpr.java` (`subEval`), `src/jsrc/org/erights/e/elang/evm/EImpl.java` (fields, `newContext`)
- **Runtime path traced:** `ObjectExpr.subEval(ctx, forValue)` → evaluate auditor exprs → `PrimAudition.ask` per auditor → allocate `fields[]` → `new EImplByProxy(approvals, fields, ctx.outers(), eMethodTable())` → populate `fields[i] = myOptFieldInits[i].getRepresentation(ctx)`
- **Observed implementation (short quotes):**
  - ObjectExpr javadoc: *"Yields an object that closes over the current scope, and responds to requests by dispatching to one of its matching methods, or to a matcher if provided and no methods match."*
  - `subEval`: `Object result = new EImplByProxy(approvals, fields, ctx.outers(), eMethodTable()); myOName.testMatch(ctx, result, null); for (int i = 0; i < numFields; i++) { fields[i] = myOptFieldInits[i].getRepresentation(ctx); }`
  - EImpl javadoc + fields: *"What an object expression evaluates to."* — `private final Object[] myFields; private final Slot[] myOuters; private final EMethodTable myScript;`
- **Semantic intent from docs:** EImpl javadoc (in-repo; erights.org unreachable). The object *is* the closure; no separate "environment handle" abstraction exists at runtime.
- **What is verified:** An E object is exactly four things: audit approvals, a **per-object array of selected captured representations** (`fields`), a shared `outers` slot array, and a shared method table. Field population happens *after* allocation (note: enables self-reference/recursive capture — `myOName.testMatch` binds the object's own name before fields fill).
- **What is inferred:** `getRepresentation(ctx)` selects only the slots the object's methods actually use (pruned capture); the exact pruning algorithm was not fetched.
- **Relevant invariant:** Capture set = statically computed free variables of the method suite; the whole enclosing frame is never captured per-object — only `outers` (top-level scope) is shared wholesale.
- **Consequence for Glia:** Confirms the two-tier split Glia proposes: shared frozen prelude ↔ E's shared `myOuters`; per-object captured state ↔ E's `myFields`. Also gives a concrete trick: allocate the object first, bind its own name, then fill fields — solves recursive `defcap` self-reference without weak self-pointers.
- **Confidence:** High (two files cross-consistent)
- **Classification:** CONFIRMS CURRENT DESIGN

### Record E-2 — Method dispatch re-enters the captured frame; static layout is split from runtime values

- **System:** E-on-Java / **Repository:** kpreid/e-on-java / **Repository status:** dormant mirror / **Revision:** `a0b3b599…`
- **Files and symbols:** `src/jsrc/org/erights/e/elang/evm/EMethod.java` (`execute`), `src/jsrc/org/erights/e/elang/scope/Scope.java`, `src/jsrc/org/erights/e/elang/scope/ScopeLayout.java`
- **Runtime path traced:** `EImpl.callAll(verb,args)` → `myScript.shorten(...)` → `EMethod.execute(self, args)` → `self.newContext(myLocalCount)` → `EvalContext.make(localCount, myFields, myOuters)` → pattern-match args → `myBody.subEval(ctx, …)`
- **Observed implementation (short quotes):**
  - `Object execute(EImpl self, Object[] args) { EvalContext ctx = self.newContext(myLocalCount); for (int i = 0, max = args.length; i < max; i++) { myPatterns[i].testMatch(ctx, args[i], null); } … return myBody.subEval(ctx, true); }`
  - `EvalContext newContext(int localCount) { return EvalContext.make(localCount, myFields, myOuters); }`
  - ScopeLayout javadoc: *"Static information about the runtime representation of a Scope. A ScopeLayout and an EvalContext together form a Scope."* … *"Each EvalContext can be seen as an instantiation of a ScopeLayout."*
  - Scope javadoc: *"A ConstMap (sort of) from names (strings) to Slots. Scopes inherit from each other in a tree, so they can be used to model nesting lexical environments."*
- **Semantic intent from docs:** ScopeLayout javadoc explicitly frames name→position as compile-time and slot values as runtime.
- **What is verified:** Method bodies reach authority by plain positional reads of `myFields`/`myOuters` through a fresh EvalContext per invocation. There is no per-call authority check: reachability *is* permission.
- **What is inferred:** Names are resolved to indices before runtime (name-free dispatch inner loop); consistent with ScopeLayout's `getNoun` returning position-aware NounExprs.
- **Relevant invariant:** "Possesses authority" is decided entirely at closure-construction time; dispatch never consults the module/owner again.
- **Consequence for Glia:** Glia's `defcap` method closures reaching module state via a captured owner reference matches E exactly — but E shows the owner reference can be *just the captured slots themselves* (no back-pointer to a Defs owner is needed for dispatch). The owner back-pointer in Glia is therefore only for identity/lifetime bookkeeping, not for semantics — keep it out of the authority path.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record E-3 — Attenuation/revocation: facets sever authority by ASSIGNING a captured slot, while the closure stays alive

- **System:** E-on-Java / **Repository:** kpreid/e-on-java / **Repository status:** dormant mirror / **Revision:** `a0b3b599…`
- **Files and symbols:** `src/esrc/org/erights/e/facet/makeCaretaker.emaker` (whole file, 107 lines; sibling patterns present: `makeMembraneAuthor.emaker`, `makeGrantMgrAuthor.emaker`, `makeStoneCast.emaker`, `once.emaker`)
- **Runtime path traced:** `makeCaretaker(var underlying)` returns `[forwarder, revoker]`; forwarder's `match [verb, args] { E.call(underlying, verb, args) }`; revoker's `revoke(problem)` does `underlying := Ref.broken(problem)`
- **Observed implementation (short quotes):**
  - `def makeCaretaker(var underlying) :Tuple[near, near] { … def forwarder { … match [verb, args] { E.call(underlying, verb, args) } } def revoker { to revoke(problem) :void { try { underlying.__reactToLostClient(problem) } finally { underlying := Ref.broken(problem) } } … } return [forwarder, revoker] }`
- **Semantic intent from docs:** In-file doc comment (quoting the E FAQ): *"In one sense, the forwarder and revoker can be seen as foreseen facets of a 'revocable forwarder' composite."* And: *"this is only for 'Cooperative (with the underlying) revocability'. Uncooperative revocability requires the Membrane pattern."*
- **What is verified:** Attenuation is ordinary user code: two closures sharing one mutable captured slot. Revocation = mutating the shared slot to a broken ref. The forwarder object remains alive and reachable after revocation — **liveness and authority are decoupled by mutation, not by weak references**.
- **What is inferred:** Nothing material; the file is complete.
- **Relevant invariant:** "Kept alive" ≠ "possesses authority" is achieved *semantically* (slot now holds a broken ref) with zero runtime support beyond mutable captured slots.
- **Consequence for Glia:** This is the direct answer to the KEY QUESTION for E: E does **not** separate kept-alive from possesses-authority at the reference level — it separates them by making the captured cell's *contents* swappable. For Glia: model revocation/attenuation as swapping the value in a captured cell (RefCell contents := PoisonedCap), and reserve the weak/strong owner-reference machinery purely for Rc-cycle management, never for authority semantics. Also note the doc's warning: transparent forwarding is *cooperative* only — a Glia membrane story is a separate future track.
- **Confidence:** High (full file quoted)
- **Classification:** CONFIRMS CURRENT DESIGN

### Record E-4 — Auditors at construction; DeepFrozen check is an admitted approximation

- **System:** E-on-Java / **Repository:** kpreid/e-on-java / **Repository status:** dormant mirror / **Revision:** `a0b3b599…`
- **Files and symbols:** `ObjectExpr.java` (`PrimAudition.ask`), `src/jsrc/org/erights/e/elib/ref/Ref.java` (`isDeepFrozen`)
- **Runtime path traced:** `subEval` → `PrimAudition.ask(auditor)` → `auditor.audit(this)` → cache verdict iff `Ref.isDeepFrozen(auditor)` → approvals stamped into `EImplByProxy(approvals, …)`
- **Observed implementation (short quotes):**
  - `if (auditor.audit(this)) { if (Ref.isDeepFrozen(auditor)) { … myOptAuditorCache.addElement(auditor); } myApprovers.push(auditor); }`
  - `static public boolean isDeepFrozen(Object ref) { return isDeepPassByCopy(ref, null); }` — with an in-source comment that it *"will make errors of omission (the safe kind)."*
- **Semantic intent from docs:** The approvals array is the object's unforgeable property certificate, checked later by `auditedBy`-style queries.
- **What is verified:** Auditing happens exactly once, at construction, and the verdict is baked immutably into the object. E-on-Java's transitive-immutability check is a conservative stand-in, not a full structural proof.
- **What is inferred:** Full DeepFrozen never landed in the Java runtime (consistent with its 2011 freeze); Monte finished this design (Record M-3).
- **Relevant invariant:** Property stamps are assigned at birth and never recomputed; conservative (deny-by-default) approximations are acceptable and safe.
- **Consequence for Glia:** Glia's `is_authority_free` may ship as a conservative under-approximation (fail closed) without breaking soundness — E did exactly that for a decade. But decide *when* it runs: E/Monte both prove at construction, not at export/call time.
- **Confidence:** Medium-high (quote of comment is paraphrase-adjacent from summarizer)
- **Classification:** REQUIRES A SPIKE (construction-time vs export-time checking in Glia)

### Record E-5 — Vats: authority-carrying objects are confined to a turn-based event loop

- **System:** E-on-Java / **Repository:** kpreid/e-on-java / **Repository status:** dormant mirror / **Revision:** `a0b3b599…`
- **Files and symbols:** `src/jsrc/org/erights/e/elib/vat/Vat.java` (`qSendMsg`, `qSendAll`, fields)
- **Runtime path traced:** eventual send → `qSendAll(rec, nowFlag, verb, args, resolver)` → `new PendingDelivery(...)` → `getRunner().enqueue(pe)` → Runner drains one delivery at a time
- **Observed implementation (short quotes):**
  - Javadoc: *"A Vat is a disjoint partitioning of objects. Each object should ideally be associated with exactly one Vat, and should only be invoked inside that Vat."*
  - `public Throwable qSendMsg(Object rec, Message msg) { PendingDelivery todo = new PendingDelivery(this, rec, false, msg); return getRunner().enqueue(todo); }`
- **Semantic intent from docs:** Vat javadoc as above.
- **What is verified:** Reentrancy discipline is *turns*: within a turn, calls are ordinary synchronous stack calls into lexical frames; cross-boundary interaction is queued.
- **What is inferred:** No borrow-like aliasing protection exists within a turn (JVM shared heap; discipline only).
- **Relevant invariant:** One object ↔ one vat; authority never migrates between event loops except via refs/messages.
- **Consequence for Glia:** Glia's single-threaded Rc/RefCell world is one vat. RefCell double-borrow panics are Glia's analogue of intra-turn reentrancy hazards E simply tolerates; if `defcap` methods can call back into their owning module mid-borrow, Glia needs a plan E doesn't provide.
- **Confidence:** High
- **Classification:** FUTURE TRACK

---

## SYSTEM 2: MONTE / TYPHON

### Record M-1 — User objects: immutable script + immutable pruned frame array

- **System:** Monte (Typhon VM)
- **Repository:** https://github.com/monte-language/typhon
- **Repository status:** Active (last commit 2025-10-22)
- **Revision or commit:** `92d70fbcbe1291f1aa7c5cedca90345b8a95f6cc`
- **Files and symbols:** `typhon/nano/interp.py` (`InterpObject`, object-construction visitor, `recvNamed`)
- **Runtime path traced:** ObjectExpr evaluation → read `script.layout.frameTable` → build `frame` by copying bindings out of the *current* scope → `InterpObject(script, frame)`; dispatch: `recvNamed(atom, …)` → `getMethod(atom)` in `self.script.methods` → `runMethod` with evaluator seeded from `self.frame`
- **Observed implementation (short quotes):**
  - `class InterpObject(Object): """ An object whose script is executed by the AST evaluator. """`
  - `_immutable_fields_ = "frame[*]", "script", "report"` … `def __init__(self, script, frame): self.script = script; self.frame = frame`
  - Construction: `frame = [self.lookupBinding(scope, index) for (_, scope, index, _) in frameTable.frameInfo]` … `o = InterpObject(script, frame)`
  - `def recvNamed(self, atom, args, namedArgs): method = self.getMethod(atom); if method is not None: return self.runMethod(method, args, namedArgs)`
- **Semantic intent from docs/papers:** monte docs `semantics.rst`/`auditors.rst` (pinned monte repo): objects certify properties structurally; see M-3.
- **What is verified:** The closure is a flat **copied array** of exactly the bindings named in the static frame table — not a pointer to the enclosing environment. `frame` is declared RPython-immutable after construction.
- **What is inferred:** Mutable captured variables are represented as captured slot/binding objects inside the frame (deslotification pass chooses severity), so mutation goes through the shared binding, not by replacing frame entries.
- **Relevant invariant:** After construction, an object's authority set is a fixed-size immutable array; the enclosing frame can die independently.
- **Consequence for Glia:** Strongly supports copy-out capture over environment-pointer capture. If Glia closures copy `Val`s (and share cells only for mutated captures), the closure needs **no** strong link to its owning `Defs` at all for evaluation — the weak/strong owner barrier shrinks to a niche concern (see M-2).
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record M-2 — Static scope analysis: OUTER vs FRAME vs LOCAL; outers deliberately excluded from closures

- **System:** Monte (Typhon) / **Repository:** monte-language/typhon / **Status:** active / **Revision:** `92d70fbc…`
- **Files and symbols:** `typhon/nano/scopes.py` (`SCOPE_OUTER/SCOPE_FRAME/SCOPE_LOCAL`, `ScopeFrame`, `FrameTable`, `ScopeItem.find`); `typhon/scopes/{safe,unsafe,boot}.py` (the split foundation)
- **Runtime path traced:** compile passes: *"Discovering the static scope layout / Removing meta.context() / Removing meta.state() / Laying out specialized frames / Deslotification"* → each name use resolved by `ScopeItem.find()` to (scope-kind, index, severity)
- **Observed implementation (short quotes):**
  - `SCOPE_OUTER, SCOPE_FRAME, SCOPE_LOCAL = makeEnum(u"scope", [u"outer", u"frame", u"local"])`
  - `SEV_NOUN, SEV_SLOT, SEV_BINDING = makeEnum(u"severity", [u"noun", u"slot", u"binding"])`
  - `ScopeFrame` tracks `frameNames` ("Names closed over") and `outerNames` — *"Names from outer scope used (not included in closure at runtime)"*
- **Semantic intent from docs:** The safe scope (`typhon/scopes/safe.py`) is the DeepFrozen ambient foundation; `unsafe.py` holds authority-bearing entry points handed only to the entrypoint module.
- **What is verified:** Three-tier storage is decided statically; outer (prelude/safe-scope) names are resolved against a shared table and cost closures nothing; per-object frames contain only mid-tier captures. Severity (`noun/slot/binding`) statically chooses value-copy vs shared-cell vs full-binding representation per name.
- **What is inferred:** This is the mature version of E's outers/fields split, with the added "severity" refinement.
- **Relevant invariant:** A frozen shared foundation + per-object pruned frame + per-call locals; capture representation (value vs cell) is chosen per-name by static analysis of assignment.
- **Consequence for Glia:** Glia's "frozen prelude inherited by fresh Defs owners" is this exact architecture. Adopt the severity idea: capture immutable names by value (no cell, no owner edge, trivially authority-analyzable) and only mutated names as shared cells — this shrinks the set of edges the weak/strong write barrier must manage and makes `is_authority_free` mostly a value-walk.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record M-3 — DeepFrozen: authority-freeness is PROVEN statically from the guards of the closure's free names — Monte's answer to "kept alive ≠ possesses authority"

- **System:** Monte (Typhon) / **Repository:** monte-language/typhon / **Status:** active / **Revision:** `92d70fbc…`
- **Files and symbols:** `typhon/objects/auditors.py` (`DeepFrozen.audit`, `auditDeepFrozen`, `deepFrozenSupersetOf`, `auditedBy`), `typhon/objects/user.py` (`Audition.ask`, `Audition.getGuard`, `prepareReport`)
- **Runtime path traced:** object with `as DeepFrozen` → Audition created with the object's **AST + static guard info** → `DeepFrozen.audit` → `auditDeepFrozen(audition)`: walk `ss.read + ss.set` of the script's static scope; for each free name `audition.getGuard(name)`; require `deepFrozenSupersetOf(guard)` → verdict cached keyed on the guard log, stamped into the object's `report`
- **Observed implementation (short quotes):**
  - `ss = ast._script.getStaticScope(); namesUsed = ss.read + ss.set … guard = audition.getGuard(name); if not deepFrozenSupersetOf(guard): errors.append(u'"%s" in the lexical scope of %s does not have a guard implying DeepFrozen, but %s' …)`
  - `Audition.getGuard`: `answer = self.guardInfo.getGuard(name) … if answer.auditedBy(deepFrozenStamp): … self.guardLog.append((name, answer)) else: … self.guardLog = None`
  - Cache invalidation in `ask`: `for name, value in guards: if self.guardInfo.getGuard(name) != value: … "Invalidating"`
  - `class DeepFrozen(Object): """ Auditor and guard for transitive immutability. """`
- **Semantic intent from docs/papers:** monte-language/monte @ `07fb0d69…` `docs/source/auditors.rst`: *"The `DeepFrozen` auditor proves that objects are immutable and that the objects they refer to are also `DeepFrozen`."* / *"For any `DeepFrozen` object, all bindings referenced by the object are also `DeepFrozen`."* / auditors examine *"an expanded expression"* and *"the slot guards of names"*; *"The auditor subsystem allows objects to certify themselves as having certain properties."*
- **What is verified:** The proof is **per-name over the closure's static free-variable set**, using guard metadata — the runtime never deep-walks the live frame for the static path (a runtime `checkDeepFrozen` walk exists as fallback for transparent/uncalled objects). An object may hold arbitrarily large live structure and still be certified authority-free, because the certificate is about *what its names can denote*, not about what its memory keeps alive.
- **What is inferred:** `deepFrozenSupersetOf` enumerates primitive guard types and recurses through composite guards (summarizer paraphrase for that one function; the two functions around it are verbatim).
- **Relevant invariant:** "Possesses authority" is a property of the closure's *name set and their guarantees*; "kept alive" is a property of the GC graph. Monte cleanly separates them: the stamp (`report`) travels with the object; liveness is RPython GC's business.
- **Consequence for Glia:** This is the strongest external validation of splitting `is_authority_free` from ownership/liveness. But Monte's proof leans on per-binding **guard metadata** that Glia doesn't have. Spike: give Glia bindings a cheap static tag (e.g. "frozen-prelude / frozen-value / cell / capability") recorded in `Defs` at definition time, so an exported closure can be certified by walking its free-name tags instead of deep-walking values through RefCells at export time.
- **Confidence:** High on mechanism; medium on `deepFrozenSupersetOf` details
- **Classification:** REQUIRES A SPIKE

### Record M-4 — Modules: DeepFrozen singleton functions from imports-map to exports-map

- **System:** Monte (Typhon) / **Repository:** monte-language/typhon (+ monte docs) / **Status:** active / **Revision:** `92d70fbc…` (docs @ `07fb0d69…`)
- **Files and symbols:** `typhon/importing.py` (`ModuleCache`, `obtainModule`, `eval`), monte `docs/source/modules.rst`
- **Runtime path traced:** `obtainModule(libraryPaths, recorder, filePath)` → compile → `moduleCache.cache[path] = code` (code cached, not instance) → per use: `eval(self, env): return evalMonte(self.astSource, env, self.origin, False)` with the import environment supplied by the loader
- **Observed implementation (short quotes):**
  - `class ModuleCache(object): """ A necessary evil. """`
  - `if path in moduleCache.cache: log.log(["import"], u"Importing %s (cached)" …); return moduleCache.cache[path]`
- **Semantic intent from docs/papers (modules.rst, pinned):**
  - *"Under the hood, modules are compiled to be DeepFrozen singleton objects which accept a mapping of imported objects, and return a mapping of exported names."*
  - *"All exports must pass `DeepFrozen`: exports can only depend on `DeepFrozen` imports."*
  - *"Module loaders will check that module exports are immutable by guarding them with `Map[Str, DeepFrozen]`. This is crucial for enforcing module isolation."*
  - *"Module parameters are injected dependencies, in the sense of dependency injection."*
- **What is verified:** What is cached is the compiled DeepFrozen module *function*; instantiation (application to an imports map) is per-composition. Exports are ordinary maps, gate-checked at the boundary.
- **What is inferred:** Enforcement of the `Map[Str, DeepFrozen]` guard lives in the loader written in Monte (mast/prelude), not in the RPython excerpts fetched.
- **Relevant invariant:** Sharing the *definition* is safe because it is proven authority-free; every instantiation's authority comes only from its argument map.
- **Consequence for Glia:** Exactly Glia's "exports = ordinary maps" and per-import instantiation. The delta worth stealing: Monte gate-checks the export map (`Map[Str, DeepFrozen]`), meaning stateful/capability-bearing modules are expressed as *exported maker functions*, not as authority-bearing exports. Glia's `defcap` deliberately relaxes this — so Glia must decide per-export which regime applies, and the export gate is where `is_authority_free` earns its keep.
- **Confidence:** High (docs pinned + loader code)
- **Classification:** CONFIRMS CURRENT DESIGN

### Record M-5 — Vats and promises: authority transfer is queued, single-turn execution

- **System:** Monte (Typhon) / **Repository:** monte-language/typhon / **Status:** active / **Revision:** `92d70fbc…`
- **Files and symbols:** `typhon/vats.py` (`Vat`, `send`, `sendOnly`, `takeTurn`, `takeSomeTurns`), `typhon/objects/refs.py` (`makePromise`, used by `send`)
- **Runtime path traced:** `send(target, atom, args, namedArgs)` → `makePromise()` → append `(resolver, target, atom, args, namedArgs)` to `self._pending` → `takeTurn()` pops one tuple and executes it; `takeSomeTurns` interleaves with `runEvents()`
- **Observed implementation (short quotes):**
  - `class Vat(Object): """ Turn management and object isolation. """`
  - `def send(self, target, atom, args, namedArgs): from typhon.objects.refs import makePromise; promise, resolver = makePromise(); with self._pendingLock: self._pending.append((resolver, target, atom, args, namedArgs)); return promise`
  - `def takeTurn(self): with self._pendingLock: resolver, target, atom, args, namedArgs = self._pending.pop(0)`
- **Semantic intent from docs:** docstring above; monte `vats.rst` not fetched.
- **What is verified:** Same turn discipline as E; promises are allocated at send time and resolved after the callee's turn.
- **What is inferred:** Object isolation between vats is discipline plus proxies (not fetched at this revision).
- **Relevant invariant:** Within a turn, synchronous lexical reachability; across turns, only queued messages and refs.
- **Consequence for Glia:** As E-5. Not needed for PR-1's module/ownership design; relevant when Glia meets Wetware's process boundary.
- **Confidence:** High
- **Classification:** FUTURE TRACK

---

## SYSTEM 3: NEWSPEAK

### Record N-1 — Class/module bodies: slots are never referenced directly; all access is accessor-method sends

- **System:** Newspeak
- **Repository:** https://github.com/newspeaklanguage/newspeak
- **Repository status:** Active (last commit 2026-07-24)
- **Revision or commit:** `945b81e80d5940ccca1780144c58dc416666ed86`
- **Files and symbols:** `BankAccount.ns` (whole file)
- **Runtime path traced:** `class BankAccount balance: b = Object new ( | balance_slot <Integer> ::= b. | ) ( … )` — factory arg `b` flows into slot init; methods use `balance` (getter send) and `balance_slot::` (setter send)
- **Observed implementation (short quotes):**
  - `class BankAccount balance: b <Integer> … = Object new ( | balance_slot <Integer> ::= b. | ) (`
  - `public balance = ( ^balance_slot )`
  - `public withdraw: amount <Integer> … = ( amount > balance ifTrue: [ Error signal: … ]. balance_slot:: balance - amount )`
- **Semantic intent from docs/papers ("Modules as Objects in Newspeak", Bracha et al., ECOOP 2010 — PDF fetched from bracha.org and read directly):** *"As in Self, there is no way to directly reference a slot … since the only operation allowed is method invocation. Slot declarations implicitly introduce accessor methods."* And: *"The use of = signifies that these are immutable slots, that will not be changed after they are initialized. No setter methods are generated for immutable slots, thus enforcing immutability."*
- **What is verified:** Storage is structural (named slots on the object) but *semantically invisible*: every read/write, including the object's own, is a virtual send. Immutability is enforced by not generating a setter.
- **What is inferred:** Access-modifier checking (`public`/`private`) is the attenuation mechanism at this layer.
- **Relevant invariant:** Representation independence: possession of an object never grants slot access, only method access.
- **Consequence for Glia:** Challenges one habit in the Glia sketch: if module `Defs` maps are readable as plain maps by anyone holding them, holding a module = reading all its state. Newspeak says: make the *export surface* the only readable face; the `Defs` map should be internal, with exported names projected through an ordinary (frozen) map — which Glia's "exports = ordinary maps" already does, provided the live `Defs` owner itself is never handed out.
- **Confidence:** High (whole file)
- **Classification:** CONFIRMS CURRENT DESIGN

### Record N-2 — Modules: top-level classes, zero global namespace, imports = factory arguments stored in immutable slots

- **System:** Newspeak / **Repository:** newspeaklanguage/newspeak / **Status:** active / **Revision:** `945b81e8…`
- **Files and symbols:** `CounterApp.ns` (`packageUsing: manifest`), `CounterUI.ns` (`usingPlatform: p`, nested `CounterPresenter`)
- **Runtime path traced:** link time: `class CounterApp packageUsing: manifest = Object new` with `private CounterUI = manifest CounterUI.`; run time: `main:args:` does `ui CounterUI usingPlatform: platform` — fresh module instance wired with an explicit platform capability
- **Observed implementation (short quotes):**
  - `class CounterApp packageUsing: manifest = Object new` / `private CounterUI = manifest CounterUI.`
  - `class CounterUI usingPlatform: p = Object new (` / `private Subject = p hopscotch Subject. private Presenter = p hopscotch Presenter.`
  - Nested: `class CounterPresenter onSubject: s <CounterSubject> = Presenter onSubject: s (` — the superclass expression `Presenter` is itself a send resolving to the module's slot.
- **Semantic intent from docs/papers (ECOOP 2010, read directly):** *"Top level classes act as module definitions, which are independent, immutable, self-contained parametric namespaces. They can be instantiated into modules which may be stateful and mutually recursive."* / *"Because there is no global namespace, a top level class declaration cannot refer to an enclosing scope; all names used within it must be defined by it or inherited."* / *"Module definitions are stateless … any connection to the world outside the module must come via parameters supplied at instance creation. Modules therefore act as sandboxes, providing a natural fit with object-capability based security."* / *"The slot definition of List fills the role of an import statement."* / *"Different module instances do not interfere with each other, because there is no static state. Module definitions are therefore re-entrant."*
- **What is verified:** Imports are literally constructor arguments, destructured into immutable slots in the instance initializer; even the superclass of a nested class is a late-bound send to such a slot. Two linkage phases exist: `packageUsing:` (manifest/link-time, may grab class *definitions*) and `usingPlatform:` (instantiation-time, grants live platform capability).
- **What is inferred:** The paper's statement that factory parameters are only in scope in the initializer (verified on p.6: *"the parameters to the factory method are only in scope within the instance initializer"*) forces all retained authority to be visible as slots — an auditable authority manifest per module.
- **Relevant invariant:** A module instance's total authority = exactly its slot contents; the slot list at the top of the class *is* the authority manifest.
- **Consequence for Glia:** Confirms per-import fresh instantiation and constructive grants (matches the approved child-bootstrap design: `create_child(image, grants)`). Steal the two-phase split: Glia module *definitions* (authority-free, shareable, ≈ frozen prelude + code) vs module *instances* (fresh `Defs` owner + explicit grant map at instantiation). Also steal "factory args scoped to initializer only": in Glia, grants passed at instantiation should have to be explicitly bound into the module's `Defs` to persist — accidental ambient retention becomes syntactically impossible.
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN

### Record N-3 — Outer references at runtime: enclosing_object chain on Behavior; implicit-receiver sends climb it; closures capture activations

- **System:** Newspeak (Primordial Soup VM — the repo's production VM)
- **Repository:** https://github.com/newspeaklanguage/primordialsoup
- **Repository status:** Active-slow (last commit 2024-08-24)
- **Revision or commit:** `9ac43beaca8b006dce62ccd237dfb2f3f3c833c5`
- **Files and symbols:** `vm/interpreter.cc` (`ImplicitReceiverSendMiss`, `OuterSendMiss`, `LexicalSend`, `PushClosure`), `vm/object.h` (`Behavior::enclosing_object()`, `AbstractMixin::enclosing_mixin()`, `Closure::defining_activation`, `Activation` fields)
- **Runtime path traced:** implicit-receiver send with no local match → `ImplicitReceiverSendMiss`: loop `candidate_receiver = candidateMixinApplication->enclosing_object(); target_mixin = target_mixin->enclosing_mixin();` until a matching method or nil; explicit `outer` send → `OuterSendMiss` loops `while count < depth`; block creation → `PushClosure`: `result->set_defining_activation(FrameActivation(fp_))`
- **Observed implementation (short quotes):**
  - `candidate_receiver = candidateMixinApplication->enclosing_object(); target_mixin = target_mixin->enclosing_mixin();`
  - `Behavior mixin_application = FindApplicationOf(mixin, receiver_class);` (lookup starts from the *defining mixin's* application, not the receiver's dynamic class alone)
  - `result->set_defining_activation(FrameActivation(fp_)); result->set_initial_bci(FrameMethod(fp_)->BCI(ip_));`
  - object.h: `"Object enclosing_object() const"` on Behavior; `Activation` fields `sender_ / method_ / closure_ / receiver_ / temps_[kMaxTemps]`; `Closure` has `"Object copied(intptr_t index) const"` for copied captures.
- **Semantic intent from docs/papers (ECOOP 2010):** *"This method is defined implicitly in class ShapeLibrary by the declaration of the nested class Shape … This is how an enclosing class provides a namespace for its nested classes."* — plus the newspeak repo's own `RuntimeForJS.ns` mirroring the same model in JS: `public kernel = Object enclosingObject.` and `Mirrors usingPlatform: self runtime: outer RuntimeForJS vmMirror: vmmirror`.
- **What is verified:** The "owner reference" is concrete: every mixin application (class-as-instantiated) holds a **strong `enclosing_object_` pointer**, and unqualified name resolution is a *dispatch* that walks that chain; block closures additionally hold a strong `defining_activation` plus copied captures. There is no weak edge anywhere in the authority path.
- **What is inferred:** Since psoup has its own tracing GC, the strong enclosing chain never causes leaks the way Rc cycles would; the chain length is the nesting depth (small).
- **Relevant invariant:** Outer access is (a) mediated by method lookup (access modifiers can attenuate it), (b) strong, (c) bounded by lexical nesting depth.
- **Consequence for Glia:** Two-sided. (1) CONFIRMS: Glia's "closure holds an owner reference; `defcap` method closures may reach module state" is exactly Newspeak's `enclosing_object` + mediated lookup — and Newspeak shows mediation (only exported/`public` names resolvable from outside, everything resolvable from inside) is what keeps this capability-safe. (2) GC caveat: Newspeak's edges are *all strong* under tracing GC; Glia's weak-at-rest/strong-when-escaped barrier has no precedent in any of the three systems — it is compensating for Rc, not for capability semantics, and must never change what names resolve (a weak edge that can go dead must be unobservable in authority terms, or it silently revokes).
- **Confidence:** High on structure; medium on exact loop bounds (summarized)
- **Classification:** NOT TRANSFERABLE — GC DIFFERENCE (for the strong-edge liveness model); the mediation/lookup structure itself CONFIRMS CURRENT DESIGN

### Record N-4 — Direct answer: module = structural object with owned slots + inherited/injected dependencies, not a name→value map

- **System:** Newspeak / **Repositories:** newspeaklanguage/newspeak @ `945b81e8…`; paper read directly (bracha.org/newspeak-modules.pdf)
- **Files and symbols:** ECOOP 2010 paper §1–2.1 (Fig. 1, Fig. 2); `CounterUI.ns`; `RuntimeForJS.ns` (`Platform` class: `public kernel = Object enclosingObject.`)
- **Runtime path traced:** Name resolution for any in-module identifier = virtual send → (self method?) → accessor of slot → else climb enclosing chain (N-3). "Exports" = the public accessors/classes; "the map" never exists as a datum.
- **Observed implementation (short quotes):** as N-1/N-2/N-3; plus paper: *"Newspeak class declarations can be nested … a Newspeak class can have three kinds of members: slots, methods and classes."* / *"class declarations implicitly introduce accessor methods for the classes. This implies that classes are first class values, and that class names are dynamically bound, and subject to override just like methods."*
- **Semantic intent:** *"Top level classes serve as module definitions, and their instances are modules."* (paper §1); modules are *"independent, immutable, self-contained parametric namespaces"* (abstract).
- **What is verified:** In Newspeak the namespace *is* the method dictionary — a name→(accessor)method table — backed by structurally owned slots. So the honest answer is: it is **both**, layered: name→member table for resolution (late-bound, overridable, access-controlled) over owned structural storage for state, with dependencies injected once and owned thereafter.
- **What is inferred:** Nothing beyond the layering itself.
- **Relevant invariant:** Resolution layer (names, late-bound, attenuable) is strictly separated from storage layer (slots, owned, invisible).
- **Consequence for Glia:** Glia's `Defs` (name→Val) can serve as *both* layers only if reads are mediated. Recommended shape from this study: `Defs` = private storage owned by the module instance; export map = a frozen name→Val projection built at module close (Monte's `Map[Str, DeepFrozen]` moment; Newspeak's `public` filter); closures resolve through their captured pruned frame (E/Monte) with the owner edge used for `defcap` self-state only (Newspeak). That keeps "kept alive" (storage layer, Rc edges) formally separate from "possesses authority" (what the resolution layer will yield — attenuable by projection and revocable by cell mutation, per E-3).
- **Confidence:** High
- **Classification:** CONFIRMS CURRENT DESIGN (with the mediation caveat from N-1)

---

## OCAP MATRIX

| dimension | **E-on-Java** | **Monte/Typhon** | **Newspeak (+psoup VM)** |
|---|---|---|---|
| authority source | object refs; captured `myFields`/`myOuters` slot arrays; safe scope ambient | object refs; per-object immutable `frame[]`; safe scope ambient, `unsafe` scope only at entrypoint | object refs; slots injected via factory args (`usingPlatform:`); no ambient at all |
| lexical capture carries authority? | Yes — method bodies read frame slots via `EvalContext.make(locals, fields, outers)`; no per-call check | Yes — `runMethod` evaluates against stored `self.frame`; no per-call check | Yes, but mediated — every outer read is a virtual send climbing `enclosing_object_`; access modifiers filter |
| module/compartment owner | none formal (emaker eval'd in chosen scope); Vat is the compartment | module = DeepFrozen singleton fn: imports-map → exports-map; loader-gated | module = instance of a top-level class; slots own imports; definitions stateless & reentrant |
| shared foundation | universal/safe scope (`myOuters` shared array) | safe scope, DeepFrozen (`typhon/scopes/safe.py`); `SCOPE_OUTER` excluded from closures | platform object explicitly passed; nothing implicitly shared |
| mutability hardening | auditors at construction; `isDeepFrozen` = conservative approx (`isDeepPassByCopy`) | DeepFrozen proven statically from free-name guards; stamp cached on object `report` | immutable slots via `=` (no setter generated); no transitive-immutability prover |
| export form | facets returned by maker functions | `Map[Str, DeepFrozen]` guard-checked map | `public` accessors/classes on the module instance |
| capability-method representation | `EMethod` in shared `EMethodTable`; per-call fresh `EvalContext` over object's fields/outers | method in shared `script`; evaluator seeded from object's `frame[]` | method in mixin; receiver + `enclosing_object` chain; blocks hold `defining_activation` + `copied` vars |
| attenuation/wrapping | facet pattern; caretaker `match [verb,args]` forwarder; revoke by `underlying := Ref.broken(problem)`; membrane for uncooperative case | guards + auditors; wrapping objects; DeepFrozen gate at module edges | access modifiers + handing out narrower objects; module sandboxing by withholding platform parts |
| reentrancy model | vat turns; synchronous within turn | vat `_pending` queue, `takeTurn` pops one | synchronous sends; actors library (`Actors.ns`) layered above |
| lifetime/GC | JVM tracing GC; all capture strong | RPython tracing GC; `frame` strong + `_immutable_fields_` | psoup tracing GC; strong `enclosing_object_`/`defining_activation` |
| host trust boundary | curated safe scope over Java classes | RPython builtins split safe/unsafe scope files | VM mirror (`vmmirror`) passed as explicit capability into `Platform using:` |
| kept-alive vs authority distinction | not at ref level; separated **by mutation** (swap captured slot to broken ref) and by pruned capture | separated **by proof**: DeepFrozen certifies authority-freeness regardless of live size; pruned capture bounds both | separated **by mediation**: pointer exists but only `public` names resolve; plus immutable slots |
| transferable lesson | revocation = cell-content swap, never reference weakness; prune capture statically | static per-name certification beats runtime deep-walks; gate exports, share only proven-pure definitions | module authority = its slot list; two-phase (definition vs instantiation) linking; make the manifest syntactically total |

---

## Cross-system answer to the KEY QUESTION

When an exported closure/method can reach authority through private lexical state, all three systems represent it the same way at bottom — **a strong, pruned, statically-computed capture set, reached positionally at dispatch with no per-call authority check** — and none of them uses reference weakness to express authority state. They separate "kept alive" from "possesses authority" by three orthogonal, composable mechanisms Glia can adopt wholesale: (1) **mutation** — authority is the current *content* of a captured cell, revoked by swapping it (E caretaker); (2) **proof** — a construction-time certificate over the closure's free names says it *cannot* carry authority (Monte DeepFrozen ≈ `is_authority_free`); (3) **mediation** — the owner edge exists but only resolves names the owner exports (Newspeak access modifiers). Glia's weak-at-rest/strong-when-escaped owner barrier has no counterpart in any system studied — all three ride tracing GC — so it should be justified purely as Rc-cycle management and kept invisible to authority semantics (a dead weak edge must never act as an implicit revoke).

**Pinned links:**
- https://raw.githubusercontent.com/kpreid/e-on-java/a0b3b599cf267b3138eea5f5fb83f27cebd28373/src/jsrc/org/erights/e/elang/evm/ObjectExpr.java (also `EMethod.java`, `EImpl.java`, `…/elang/scope/Scope.java`, `ScopeLayout.java`, `…/elib/vat/Vat.java`, `…/elib/ref/Ref.java`, `src/esrc/org/erights/e/facet/makeCaretaker.emaker`)
- https://raw.githubusercontent.com/monte-language/typhon/92d70fbcbe1291f1aa7c5cedca90345b8a95f6cc/typhon/nano/interp.py (also `typhon/nano/scopes.py`, `typhon/objects/auditors.py`, `typhon/objects/user.py`, `typhon/importing.py`, `typhon/vats.py`)
- https://raw.githubusercontent.com/monte-language/monte/07fb0d6985454010f71a387613cec901876c545a/docs/source/auditors.rst (also `modules.rst`; fetched at `master` == this sha)
- https://raw.githubusercontent.com/newspeaklanguage/newspeak/945b81e80d5940ccca1780144c58dc416666ed86/BankAccount.ns (also `CounterApp.ns`, `CounterUI.ns`, `RuntimeForJS.ns`)
- https://raw.githubusercontent.com/newspeaklanguage/primordialsoup/9ac43beaca8b006dce62ccd237dfb2f3f3c833c5/vm/interpreter.cc (also `vm/object.h`)
- https://bracha.org/newspeak-modules.pdf (ECOOP 2010; PDF read directly, pages 1–6)