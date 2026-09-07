-------------------------------- MODULE Tasks -------------------------------
(***************************************************************************)
(* The swarm task graph (docs 12, 13) as its fold enforces it:             *)
(* aho_tasks::Graph::apply. Every Task/* event is one action carrying the  *)
(* guards the Rust fold checks, so a trace is the Task/* entries of a log. *)
(* Next offers those events over a small instance (Work ids, two agents,   *)
(* parameters drawn as the engine draws them plus fold-only moves such as *)
(* rescoping any gate); the engine emits a subset of what the fold takes.  *)
(* `Blocked` is not a status: it is derived from dependencies (Blocked).   *)
(*                                                                         *)
(*   Acyclic            depends_on never closes a cycle                    *)
(*   BlockedDerived     nothing starts or completes with an open dep       *)
(*   GateCoversScope    a passed gate's scope is done and named in covers  *)
(*   GateNotOverLow     a passed gate has no unaddressed LOW sibling       *)
(*   StaleScope         a passed gate's verdict is over its whole scope    *)
(*   ArtifactOrNothing  completed iff an artifact; one requeue, then fail  *)
(*   ReclaimCapped      reclaims <= max_reclaims; ReclaimCap is earned     *)
(*   OwnerIffRunning, OneNodePerWorker, WorkerBudget   ownership           *)
(*   VerifyNotOverFailingInvariant   a verify gate passed over no covering *)
(*                      invariant that was Fail or Stale (docs 09, 13)     *)
(*                                                                         *)
(* The invariant evidence is state too: `inv` is the latest               *)
(* Invariant/Checked per id (State.invariants) and `code` the tree of the  *)
(* latest Workspace/Checkpoint; a check of other code is Stale             *)
(* (aho_inspect::invariants). Which invariants cover a verify gate is     *)
(* decided by file paths outside the model (tool-tasks); a Completed      *)
(* event names them with the status it read, and the step holds only if   *)
(* that status is the log's.                                              *)
(*                                                                         *)
(* Not modelled: artifact text (thin artifacts and deep-mode contracts are *)
(* refused before an event is built, by HandoffArtifact::thin_reason and   *)
(* check_artifact), node content (so a re-sent Seeded of a known id is a   *)
(* stutter, the fold's idempotent case), seq, checkpoint text, salvage     *)
(* payloads, counters no guard reads (reopened, last_owner), and the       *)
(* engine's dispatch order. A completed gate keeps its dispatch snapshot   *)
(* in `scope` (the Rust node clears it; nothing reads it outside a running *)
(* gate). The settings are state (Task/Configured); Next keeps the ones    *)
(* Init sets, since a later, lower cap legitimately leaves earlier counts  *)
(* above it.                                                               *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Work,           \* model only: ids of the work nodes a model may create
    Agents,         \* model only: agent ids (AgentId + 1 in traces, all > 0)
    MaxLive,        \* swarm.max_live_workers in force from Init
    MaxMembers,     \* swarm.max_members in force from Init
    MaxReclaims,    \* swarm.max_reclaims in force from Init
    MaxKids,        \* model only: most children of one expansion
    MaxNodes,       \* model only: CONSTRAINT bound on the graph size
    MaxDepth,       \* model only: CONSTRAINT bound on the behavior length
    Invs,           \* model only: invariant ids a model may check
    Codes           \* model only: workspace trees a model may move to

VARIABLES
    node,           \* task id -> the stored node (see NewNode)
    members,        \* agents that ever took a node (swarm.max_members)
    cfg,            \* the settings in force: [maxLive, maxMembers, maxReclaims]
    inv,            \* invariant id -> [verdict, code] of its latest check
    code            \* the tree of the latest workspace checkpoint; "" before one

vars == <<node, members, cfg, inv, code>>
Evidence == <<inv, code>>

Root == "gate:root"
WorkKinds == {"explore", "code", "synthesize"}
GateKinds == {"gate:critique", "gate:verify", "gate:root"}
Statuses == {"queued", "running", "running_stale", "completed", "failed", "stopped"}
Confidences == {"low", "medium", "high"}
InvVerdicts == {"pass", "fail", "unknown", "stale", "vacuous"}
(* A verify gate cannot pass while a covering invariant is one of these.  *)
Blocking == {"fail", "stale"}

Set(s) == {s[i] : i \in DOMAIN s}

Nodes == DOMAIN node
IsGate(n) == node[n].kind \in GateKinds
IsRunning(n) == node[n].status \in {"running", "running_stale"}
IsTerminal(n) == node[n].status \in {"completed", "failed", "stopped"}
IsVerify(n) == node[n].kind = "gate:verify"

(* aho_inspect::invariants: once the workspace has a checkpoint, a check  *)
(* of any other code is Stale; before one, the verdict stands.            *)
InvStatus(i) == IF code # "" /\ inv[i].code # code THEN "stale" ELSE inv[i].verdict

(* open_deps: a dependency that is missing or not completed.             *)
Open(n) == {d \in node[n].deps : d \notin Nodes \/ node[d].status # "completed"}
Ready(n) == node[n].status = "queued" /\ Open(n) = {}
Blocked(n) == node[n].status = "queued" /\ Open(n) # {}

Live == {node[n].owner : n \in {m \in Nodes : IsRunning(m)}} \ {0}
Holder(n, a) == n \in Nodes /\ IsRunning(n) /\ node[n].owner = a
Holding(a) == {n \in Nodes : Holder(n, a)}

GateFor(k) == IF k = "code" THEN "gate:verify" ELSE "gate:critique"
GateId(p, gk) == p \o (IF gk = "gate:verify" THEN "/verify" ELSE "/critique")
OpenParent(p) == p \in Nodes /\ node[p].expanded /\ node[p].status = "queued"
Dependents(n) == {m \in Nodes : n \in node[m].deps}

(* Everything the nodes in S depend on, transitively, in graph g.        *)
RECURSIVE ClosureIn(_, _)
ClosureIn(g, S) ==
    LET T == S \cup UNION {g[d].deps : d \in S \cap DOMAIN g}
    IN IF T = S THEN S ELSE ClosureIn(g, T)

AcyclicIn(g) == \A n \in DOMAIN g : n \notin ClosureIn(g, g[n].deps)

(* Graph::reaches: `from` is `target` or transitively depends on it.      *)
Reaches(from, target) ==
    from = target \/ (from \in Nodes /\ target \in ClosureIn(node, node[from].deps))

(* Graph::inputs: direct deps plus, through each gate dep, its scope.     *)
Inputs(c) == node[c].deps \cup UNION {node[d].deps : d \in {x \in node[c].deps : x \in Nodes /\ IsGate(x)}}

(* A LOW sibling is addressed when work this gate injected for it         *)
(* completed at medium or better.                                         *)
Addressed(g, s) ==
    \E n \in Nodes :
        /\ node[n].injBy = g
        /\ s \in node[n].addr
        /\ node[n].status = "completed"
        /\ node[n].conf \in {"medium", "high"}

(* The gates a re-opened producer takes back with it.                     *)
GatesOver(p) == {g \in Nodes : IsGate(g) /\ p \in node[g].deps /\ (node[g].status = "completed" \/ IsRunning(g))}

RECURSIVE Ancestry(_, _)
Ancestry(n, fuel) ==
    IF fuel = 0 \/ node[n].parent \notin Nodes THEN {n} ELSE {n} \cup Ancestry(node[n].parent, fuel - 1)

(* Graph::in_subtree: the agent holds or planned the node or an ancestor. *)
InSubtree(n, a) == \E m \in Ancestry(n, Cardinality(Nodes)) : node[m].owner = a \/ node[m].planner = a

SafeFrom(op) ==
    CASE op = "retry" -> {"failed", "stopped"}
      [] op = "reassign" -> {"queued", "running_stale"}
      [] op = "replace" -> {"running_stale", "failed", "stopped"}
      [] op = "salvage" -> {"queued", "running_stale", "failed", "stopped"}
      [] OTHER -> {}

(* `seen`: the covering invariants (and statuses) a verify gate's pass    *)
(* named (VerifyNotOverFailingInvariant).                                 *)
NewNode(k, ds, p) ==
    [kind |-> k, parent |-> p, deps |-> ds, status |-> "queued", owner |-> 0, pinned |-> 0,
     planner |-> 0, expanded |-> FALSE, conf |-> "none", covers |-> {}, scope |-> {},
     misses |-> 0, reclaims |-> 0, failure |-> "none", injBy |-> "", addr |-> {}, gap |-> {},
     seen |-> {}]

(* check_spec_shape: a gate has a scope; nothing depends on itself.      *)
Shaped(id, k, ds) == (k \in GateKinds => ds # {}) /\ id \notin ds

(* release(): clears the holder and the dispatch snapshot.               *)
Release(r, st) == [r EXCEPT !.owner = 0, !.scope = {}, !.status = st]

-----------------------------------------------------------------------------

(* `gap` records the open dependencies the step saw (BlockedDerived).     *)
Started(id, a, sc) ==
    /\ id \in Nodes /\ node[id].status = "queued"
    /\ Open(id) = {}
    /\ node[id].pinned \in {0, a}
    /\ Holding(a) = {}
    /\ a \in Live \/ Cardinality(Live) < cfg.maxLive
    /\ a \in members \/ Cardinality(members) < cfg.maxMembers
    /\ sc = IF IsGate(id) THEN node[id].deps ELSE {}
    /\ node' = [node EXCEPT ![id] = [@ EXCEPT !.status = "running", !.owner = a, !.pinned = 0, !.scope = sc, !.gap = Open(id)]]
    /\ members' = members \cup {a}
    /\ UNCHANGED cfg

(* validate_gate first (stale scope, uncovered siblings, unaddressed LOW  *)
(* siblings, covering invariants not passing), then open dependencies.    *)
(* `seen` holds [id, status] of the invariants covering a verify gate;    *)
(* each status must be the one the log has now.                          *)
Completed(id, a, c, cov, seen) ==
    /\ Holder(id, a)
    /\ c \in Confidences
    /\ IsGate(id) => node[id].deps \subseteq node[id].scope
    /\ IsGate(id) => {d \in node[id].deps : node[d].status = "completed"} \subseteq cov
    /\ IsGate(id) => \A d \in node[id].deps :
                        (node[d].status = "completed" /\ node[d].conf = "low") => Addressed(id, d)
    /\ IsVerify(id) \/ seen = {}
    /\ \A s \in seen : s.id \in DOMAIN inv /\ s.status = InvStatus(s.id)
    /\ \A s \in seen : s.status \notin Blocking
    /\ Open(id) = {}
    /\ node' = [node EXCEPT ![id] = [@ EXCEPT !.status = "completed", !.owner = 0, !.conf = c, !.covers = cov, !.gap = Open(id),
                                              !.seen = seen]]
    /\ UNCHANGED <<members, cfg>>

(* Artifact-or-nothing, first miss.                                       *)
Requeued(id, a) ==
    /\ Holder(id, a)
    /\ node[id].misses < 1
    /\ node' = [node EXCEPT ![id] = [Release(@, "queued") EXCEPT !.misses = @ + 1]]
    /\ UNCHANGED <<members, cfg>>

Failed(id, r) ==
    /\ id \in Nodes /\ IsRunning(id)
    /\ \/ r = "no_artifact" /\ node[id].misses >= 1
       \/ r = "reclaim_cap" /\ node[id].reclaims >= cfg.maxReclaims
    /\ node' = [node EXCEPT ![id] = [Release(@, "failed") EXCEPT !.failure = r]]
    /\ UNCHANGED <<members, cfg>>

(* Dead assignee: the k-th reclaim of the node, k <= max_reclaims.        *)
Reclaimed(id, k, from) ==
    /\ id \in Nodes /\ IsRunning(id)
    /\ node[id].owner = from
    /\ k = node[id].reclaims + 1
    /\ k <= cfg.maxReclaims
    /\ node' = [node EXCEPT ![id] = [Release(@, "queued") EXCEPT !.reclaims = k]]
    /\ UNCHANGED <<members, cfg>>

Released(id, a) ==
    /\ Holder(id, a)
    /\ IsGate(id)
    /\ node[id].deps # node[id].scope
    /\ node' = [node EXCEPT ![id] = Release(@, "queued")]
    /\ UNCHANGED <<members, cfg>>

(* A gate turns a miss into work under its own parent; the gate's scope   *)
(* grows, so its running dispatch is stale.                               *)
Injected(g, n, s, a) ==
    /\ Holder(g, a)
    /\ IsGate(g)
    /\ n.origin = "gate" /\ n.kind \in WorkKinds /\ n.parent = node[g].parent
    /\ n.id \notin Nodes
    /\ Shaped(n.id, n.kind, n.deps)
    /\ n.deps \subseteq Nodes
    /\ \A d \in n.deps : ~Reaches(d, g)
    /\ s \subseteq node[g].deps
    /\ node' = [node @@ (n.id :> [NewNode(n.kind, n.deps, n.parent) EXCEPT !.injBy = g, !.addr = s])
                    EXCEPT ![g].deps = @ \cup {n.id}]
    /\ UNCHANGED <<members, cfg>>

(* A gate (or an open synthesis join) gains scope; a completed gate       *)
(* re-opens, which only a queued dependent may see.                       *)
Rescoped(id, add) ==
    /\ add # {}
    /\ id \in Nodes /\ (IsGate(id) \/ (node[id].expanded /\ node[id].status = "queued"))
    /\ add \subseteq Nodes
    /\ \A x \in add : x # id /\ x \notin node[id].deps
    /\ \A x \in add : ~Reaches(x, id)
    /\ (IsGate(id) /\ node[id].status = "completed") => \A d \in Dependents(id) : node[d].status = "queued"
    /\ node' = [node EXCEPT ![id] =
                   IF IsGate(id) /\ @.status = "completed"
                   THEN [@ EXCEPT !.deps = @ \cup add, !.status = "queued", !.conf = "none", !.covers = {}, !.scope = {}, !.seen = {}]
                   ELSE [@ EXCEPT !.deps = @ \cup add]]
    /\ UNCHANGED <<members, cfg>>

(* Only the owner, only while Running and unexpanded; the children and    *)
(* their gates arrive together, each work child under exactly one gate of *)
(* its kind, and the parent re-queues as the join over those gates.       *)
Expanded(p, ch, joins, a) ==
    LET work  == {c \in ch : c.kind \in WorkKinds}
        gates == {c \in ch : c.kind \in GateKinds}
        ids   == {c.id : c \in ch}
        grown == node @@ [i \in ids |-> LET c == CHOOSE x \in ch : x.id = i IN NewNode(c.kind, c.deps, p)]
        after == [grown EXCEPT ![p] = [@ EXCEPT !.expanded = TRUE, !.planner = a, !.deps = @ \cup joins,
                                                !.owner = 0, !.scope = {}, !.status = "queued"]]
    IN
    /\ p \in Nodes /\ ~IsGate(p)
    /\ ~node[p].expanded
    /\ node[p].status = "running"
    /\ node[p].owner = a
    /\ work # {}
    /\ \A c \in ch : c.parent = p
    /\ ids \cap Nodes = {} /\ Cardinality(ids) = Cardinality(ch)
    /\ \A c \in ch : Shaped(c.id, c.kind, c.deps)
    /\ \A c \in gates : c.origin = "gate" /\ c.kind # "gate:root" /\ c.id = GateId(p, c.kind)
    /\ \A c \in work : c.origin = "expand"
    /\ \A c \in ch : p \notin c.deps /\ c.deps \subseteq Nodes \cup ids
    /\ {g.id : g \in gates} = joins
    /\ \A g \in gates : \A d \in g.deps : \E w \in work : w.id = d /\ GateFor(w.kind) = g.kind
    /\ \A w \in work : Cardinality({g \in gates : w.id \in g.deps}) = 1
    /\ AcyclicIn(after)
    /\ node' = after
    /\ UNCHANGED <<members, cfg>>

Seeded(id, k, ds, o, p) ==
    /\ id \notin Nodes
    /\ Shaped(id, k, ds)
    /\ \/ o = "seed" /\ p = "" /\ k \in WorkKinds
       \/ o = "gap" /\ p # "" /\ k \in WorkKinds /\ OpenParent(p)
       \/ o = "gate" /\ k = "gate:root" /\ p = "" /\ id = Root
       \/ o = "gate" /\ k \in GateKinds \ {"gate:root"} /\ id = GateId(p, k) /\ OpenParent(p)
    /\ ds \subseteq Nodes
    /\ node' = node @@ (id :> NewNode(k, ds, p))
    /\ UNCHANGED <<members, cfg>>

(* Heartbeat and Checkpoint: a sign of life clears a resume's staleness.  *)
Heartbeat(id, a) ==
    /\ Holder(id, a)
    /\ node' = [node EXCEPT ![id].status = "running"]
    /\ UNCHANGED <<members, cfg>>

(* Operator control under the safe-state table; counters reset. `salv` is *)
(* "none", "empty" or "given": only Salvage carries a snapshot, non-empty. *)
Control(id, op, by, to, salv) ==
    /\ by = "operator"
    /\ id \in Nodes /\ node[id].status \in SafeFrom(op)
    /\ op = "reassign" => to # 0
    /\ IF op = "salvage" THEN salv = "given" ELSE salv = "none"
    /\ node' = [node EXCEPT ![id] = [Release(@, "queued") EXCEPT !.pinned = to, !.failure = "none", !.reclaims = 0, !.misses = 0,
                                        !.conf = IF op = "retry" THEN "none" ELSE @,
                                        !.covers = IF op = "retry" THEN {} ELSE @,
                                        !.seen = IF op = "retry" THEN {} ELSE @]]
    /\ UNCHANGED <<members, cfg>>

(* A consumer's acceptance check failed: the producer, every gate that    *)
(* passed over it (or is reviewing it) and the consumer go back to the    *)
(* queue; no other dependent of those gates may have moved on.            *)
Reopened(p, c, gs, a) ==
    /\ Holder(c, a)
    /\ ~IsGate(c)
    /\ p \in Inputs(c)
    /\ p \in Nodes /\ ~IsGate(p)
    /\ node[p].status = "completed"
    /\ gs = GatesOver(p)
    /\ \A g \in gs : \A d \in Dependents(g) : d = c \/ node[d].status = "queued"
    /\ node' = [n \in Nodes |->
                   IF n \in gs THEN [Release(node[n], "queued") EXCEPT !.conf = "none", !.covers = {}, !.seen = {}]
                   ELSE IF n = p THEN [node[n] EXCEPT !.status = "queued", !.conf = "none", !.covers = {}, !.pinned = 0]
                   ELSE IF n = c THEN Release(node[n], "queued")
                   ELSE node[n]]
    /\ UNCHANGED <<members, cfg>>

(* The operator (or engineer) stops anything; an agent its own subtree.   *)
Stopped(id, by, a) ==
    /\ id \in Nodes /\ ~IsTerminal(id)
    /\ \/ by \in {"operator", "engineer"}
       \/ by = "agent" /\ InSubtree(id, a)
    /\ node' = [node EXCEPT ![id] = Release(@, "stopped")]
    /\ UNCHANGED <<members, cfg>>

(* The session resumed: running work is stale until its worker is heard.  *)
Resumed ==
    /\ node' = [n \in Nodes |-> IF node[n].status = "running" THEN [node[n] EXCEPT !.status = "running_stale"] ELSE node[n]]
    /\ UNCHANGED <<members, cfg>>

Configured(s) ==
    /\ s.maxMembers >= 1
    /\ s.maxLive >= 1
    /\ cfg' = s
    /\ UNCHANGED <<node, members>>

(* Invariant/Checked: the latest verdict per id, about the code `c`.      *)
InvariantChecked(i, v, c) ==
    /\ v \in InvVerdicts
    /\ inv' = (i :> [verdict |-> v, code |-> c]) @@ inv
    /\ UNCHANGED <<node, members, cfg, code>>

(* Workspace/Checkpoint: every later status is judged against this tree.  *)
WorkspaceCheckpoint(t) ==
    /\ t # ""
    /\ code' = t
    /\ UNCHANGED <<node, members, cfg, inv>>

-----------------------------------------------------------------------------

(* A log starts with no checks and no checkpoint (Tasks.cfg has no Invs  *)
(* or Codes); a model may start with checks already recorded.            *)
Init ==
    /\ node = <<>>
    /\ members = {}
    /\ cfg = [maxLive |-> MaxLive, maxMembers |-> MaxMembers, maxReclaims |-> MaxReclaims]
    /\ inv \in [Invs -> [verdict : {"pass", "fail"}, code : Codes \cup {""}]]
    /\ code \in Codes \cup {""}

ModelKinds == {"explore", "code"}
Free == Work \ Nodes
Tops == {n \in Nodes : node[n].parent = "" /\ ~IsGate(n)}
AtMostOne(S) == {{}} \cup {{x} : x \in S}

(* Gap work of `p` of gate kind `gk` that no gate reviews yet.             *)
GapKids(p, gk) ==
    {n \in Nodes : node[n].parent = p /\ ~IsGate(n) /\ GateFor(node[n].kind) = gk
                   /\ \A g \in Nodes : IsGate(g) => n \notin node[g].deps}

(* Graph::expand: work children plus one gate per kind present.           *)
Kids(p, K) ==
    LET C == DOMAIN K
        gate(gk) == [id |-> GateId(p, gk), kind |-> gk, deps |-> {x \in C : GateFor(K[x]) = gk},
                     origin |-> "gate", parent |-> p]
    IN {[id |-> x, kind |-> K[x], deps |-> {}, origin |-> "expand", parent |-> p] : x \in C}
       \cup {gate(gk) : gk \in {GateFor(K[x]) : x \in C}}

Expansions(p) ==
    {Kids(p, K) : K \in UNION {[C -> ModelKinds] : C \in {C \in SUBSET Free : C # {} /\ Cardinality(C) <= MaxKids}}}

(* A gate's `covers`: its whole scope, or the scope short of one task.     *)
Verdicts(id) ==
    IF IsGate(id) THEN {node[id].deps} \cup {node[id].deps \ {d} : d \in node[id].deps} ELSE {{}}

GateIds(ch) == {c.id : c \in {x \in ch : x.kind \in GateKinds}}

(* The invariants a verify gate's files may be covered by: any recorded.   *)
Covering(id) == IF IsVerify(id) THEN SUBSET DOMAIN inv ELSE {{}}
Seen(S) == {[id |-> i, status |-> InvStatus(i)] : i \in S}

TaskNext ==
    \/ \E id \in Free, k \in ModelKinds, ds \in AtMostOne(Nodes) : Seeded(id, k, ds, "seed", "")
    \/ Seeded(Root, "gate:root", Tops, "gate", "")
    \/ \E id \in Free, k \in ModelKinds, p \in Nodes : Seeded(id, k, {}, "gap", p)
    \/ \E p \in Nodes, gk \in {"gate:critique", "gate:verify"} : Seeded(GateId(p, gk), gk, GapKids(p, gk), "gate", p)
    \/ \E id \in Nodes, x \in Nodes : Rescoped(id, {x})
    \/ \E p \in Nodes, a \in Agents : \E ch \in Expansions(p) : Expanded(p, ch, GateIds(ch), a)
    \/ \E id \in Nodes, a \in Agents : Started(id, a, IF IsGate(id) THEN node[id].deps ELSE {})
    \/ \E id \in Nodes, a \in Agents : Heartbeat(id, a)
    \/ \E id \in Nodes, a \in Agents, c \in {"low", "high"} : \E cov \in Verdicts(id) : \E S \in Covering(id) :
           Completed(id, a, IF IsGate(id) THEN "high" ELSE c, cov, Seen(S))
    \/ \E g \in Nodes, a \in Agents, id \in Free, k \in ModelKinds : \E s \in AtMostOne(node[g].deps) :
           Injected(g, [id |-> id, kind |-> k, deps |-> {}, origin |-> "gate", parent |-> node[g].parent], s, a)
    \/ \E id \in Nodes, a \in Agents : Released(id, a)
    \/ \E id \in Nodes, a \in Agents : Requeued(id, a)
    \/ \E id \in Nodes, r \in {"no_artifact", "reclaim_cap"} : Failed(id, r)
    \/ \E id \in Nodes, a \in Agents : Reclaimed(id, node[id].reclaims + 1, a)
    \/ \E id \in Nodes, op \in {"retry", "replace", "salvage"} : Control(id, op, "operator", 0, IF op = "salvage" THEN "given" ELSE "none")
    \/ \E id \in Nodes, to \in Agents : Control(id, "reassign", "operator", to, "none")
    \/ \E c \in Nodes, p \in Nodes, a \in Agents : Reopened(p, c, GatesOver(p), a)
    \/ \E id \in Nodes : Stopped(id, "operator", 0)
    \/ Resumed

(* Checks run against the code of the moment; the code moves on.          *)
Next ==
    \/ TaskNext /\ UNCHANGED Evidence
    \/ \E i \in Invs, v \in {"pass", "fail"} : InvariantChecked(i, v, code)
    \/ \E t \in Codes : WorkspaceCheckpoint(t)

Spec == Init /\ [][Next]_vars

Bound == Cardinality(Nodes) <= MaxNodes /\ TLCGet("level") <= MaxDepth

-----------------------------------------------------------------------------

AllIds == Work \cup {Root} \cup {GateId(w, gk) : w \in Work, gk \in {"gate:critique", "gate:verify"}}
Agent0 == Agents \cup {0}

TypeOK ==
    /\ Nodes \subseteq AllIds
    /\ members \subseteq Agents
    /\ cfg = [maxLive |-> MaxLive, maxMembers |-> MaxMembers, maxReclaims |-> MaxReclaims]
    /\ DOMAIN inv \subseteq Invs
    /\ \A i \in DOMAIN inv : inv[i] \in [verdict : InvVerdicts, code : Codes \cup {""}]
    /\ code \in Codes \cup {""}
    /\ \A n \in Nodes :
         node[n] \in [kind : WorkKinds \cup GateKinds, parent : AllIds \cup {""}, deps : SUBSET Nodes,
                      status : Statuses, owner : Agent0, pinned : Agent0, planner : Agent0,
                      expanded : BOOLEAN, conf : Confidences \cup {"none"}, covers : SUBSET AllIds,
                      scope : SUBSET Nodes, misses : Nat, reclaims : Nat,
                      failure : {"none", "no_artifact", "reclaim_cap"}, injBy : AllIds \cup {""},
                      addr : SUBSET Nodes, gap : SUBSET Nodes, seen : SUBSET [id : Invs, status : InvVerdicts]]

Acyclic == AcyclicIn(node)

(* Blocked is derived, never stored: no step started or completed a node  *)
(* while one of its dependencies was open.                                *)
BlockedDerived == \A n \in Nodes : node[n].gap = {}

PassedGates == {g \in Nodes : IsGate(g) /\ node[g].status = "completed"}

GateCoversScope ==
    \A g \in PassedGates : \A d \in node[g].deps : node[d].status = "completed" /\ d \in node[g].covers

GateNotOverLow ==
    \A g \in PassedGates : \A d \in node[g].deps : node[d].conf = "low" => Addressed(g, d)

StaleScope == \A g \in PassedGates : node[g].scope = node[g].deps

ArtifactOrNothing ==
    \A n \in Nodes :
        /\ (node[n].status = "completed") <=> (node[n].conf # "none")
        /\ node[n].misses <= 1
        /\ node[n].failure # "none" => node[n].status = "failed"
        /\ node[n].failure = "no_artifact" => node[n].misses = 1

ReclaimCapped ==
    \A n \in Nodes :
        /\ node[n].reclaims <= cfg.maxReclaims
        /\ node[n].failure = "reclaim_cap" => node[n].reclaims >= cfg.maxReclaims

OwnerIffRunning == \A n \in Nodes : (node[n].owner # 0) <=> IsRunning(n)

OneNodePerWorker == \A a \in Live : Cardinality(Holding(a)) = 1

WorkerBudget ==
    /\ Live \subseteq members
    /\ Cardinality(Live) <= cfg.maxLive
    /\ Cardinality(members) <= cfg.maxMembers

(* Docs 09, 13: a verify gate cannot pass while an invariant covering it  *)
(* is Fail or Stale; what a passed verify gate saw was neither.           *)
VerifyNotOverFailingInvariant ==
    \A g \in PassedGates : IsVerify(g) => \A s \in node[g].seen : s.status \notin Blocking

TraceInv ==
    /\ Acyclic /\ BlockedDerived /\ GateCoversScope /\ GateNotOverLow /\ StaleScope
    /\ ArtifactOrNothing /\ ReclaimCapped /\ OwnerIffRunning /\ OneNodePerWorker /\ WorkerBudget
    /\ VerifyNotOverFailingInvariant

-----------------------------------------------------------------------------

(* One record per entry (aho_formal::trace_for "Tasks", Task/* bodies     *)
(* through aho_tasks::tla_record): `task` is the node the event names,    *)
(* agents are AgentId + 1 (0 is nobody), a seeded task's parent is       *)
(* `under` (absent: ""; `parent` is the entry's), lists are              *)
(* tuples. A Task/* body that does not decode is TaskUnreadable, no step. *)
(* Invariant/Checked and Workspace/Checkpoint entries carry `inv`,        *)
(* `verdict`, `code` and `tree` (hashes in hex).                          *)
Spec1(c) == [id |-> c.id, kind |-> c.kind, deps |-> Set(c.deps), origin |-> c.origin, parent |-> c.parent]

TaskEvents == {"Configured", "Seeded", "Rescoped", "Expanded", "Started", "Heartbeat", "Checkpoint",
               "Completed", "Injected", "Released", "Requeued", "Failed", "Reclaimed", "Control",
               "Reopened", "Stopped", "Resumed", "TaskUnreadable"}

TaskStep(e) ==
    \/ e.action = "Configured" /\ Configured([maxLive |-> e.maxLive, maxMembers |-> e.maxMembers, maxReclaims |-> e.maxReclaims])
    \/ e.action = "Seeded" /\ e.task \in Nodes /\ UNCHANGED vars
    \/ e.action = "Seeded" /\ Seeded(e.task, e.kind, Set(e.deps), e.origin, e.under)
    \/ e.action = "Rescoped" /\ Rescoped(e.task, Set(e.add))
    \/ e.action = "Expanded" /\ Expanded(e.task, {Spec1(c) : c \in Set(e.children)}, Set(e.joins), e.agent)
    \/ e.action = "Started" /\ Started(e.task, e.agent, Set(e.scope))
    \/ e.action \in {"Heartbeat", "Checkpoint"} /\ Heartbeat(e.task, e.agent)
    \/ e.action = "Completed" /\ Completed(e.task, e.agent, e.conf, Set(e.covers), Set(e.invs))
    \/ e.action = "Injected" /\ Injected(e.task, Spec1(e.new), Set(e.addresses), e.agent)
    \/ e.action = "Released" /\ Released(e.task, e.agent)
    \/ e.action = "Requeued" /\ Requeued(e.task, e.agent)
    \/ e.action = "Failed" /\ Failed(e.task, e.reason)
    \/ e.action = "Reclaimed" /\ Reclaimed(e.task, e.n, e.agent)
    \/ e.action = "Control" /\ Control(e.task, e.op, e.by, e.to, e.salvage)
    \/ e.action = "Reopened" /\ Reopened(e.task, e.consumer, Set(e.gates), e.agent)
    \/ e.action = "Stopped" /\ Stopped(e.task, e.by, e.agent)
    \/ e.action = "Resumed" /\ Resumed

EvidenceEvents == {"InvariantChecked", "WorkspaceCheckpoint"}

TraceStep(e) ==
    \/ e.action \in TaskEvents /\ TaskStep(e) /\ UNCHANGED Evidence
    \/ e.action = "InvariantChecked" /\ InvariantChecked(e.inv, e.verdict, e.code)
    \/ e.action = "WorkspaceCheckpoint" /\ WorkspaceCheckpoint(e.tree)
    \/ e.action \notin TaskEvents \cup EvidenceEvents /\ UNCHANGED vars

=============================================================================
