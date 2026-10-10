--------------------------- MODULE MarginReview ---------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Paths, Scope, Values, InitialValue, RequestIds,
          MaxCommits, Cooperative

NoRequest == "no-request"
ASSUME /\ IsFiniteSet(Paths) /\ Paths # {}
       /\ IsFiniteSet(Values) /\ InitialValue \in Values
       /\ Scope \subseteq Paths /\ Scope # {}
       /\ IsFiniteSet(RequestIds) /\ RequestIds # {}
       /\ NoRequest \notin RequestIds
       /\ MaxCommits \in Nat /\ MaxCommits >= 2
       /\ Cooperative \in BOOLEAN

Trees == [Paths -> Values]
InitialTree == [p \in Paths |-> InitialValue]
Active == {"apply", "index", "commit", "respond"}
Phases == Active \cup {"idle", "down", "recovery"}

Commit(t, k, r, b, c, rs, done) ==
  [tree |-> t, kind |-> k, req |-> r, base |-> b,
   cand |-> c, restore |-> rs, complete |-> done]
Plan(r, b, c, rs, done) ==
  [req |-> r, base |-> b, cand |-> c, restore |-> rs,
   complete |-> done]
Reply(k, r, b, c, rs, done, oid) ==
  [code |-> k, req |-> r, base |-> b, cand |-> c,
   restore |-> rs, complete |-> done, oid |-> oid]
NoPlan == Plan(NoRequest, 0, 0, {}, FALSE)
NoReply == Reply("none", NoRequest, 0, 0, {}, FALSE, 0)

VARIABLES history, baseline, work, index, phase, pending,
          remaining, repoReady, reply, unseenOverwrite
vars == <<history, baseline, work, index, phase, pending,
          remaining, repoReady, reply, unseenOverwrite>>

HeadId == Len(history)
HeadTree == history[HeadId].tree
ReviewIds == {i \in 1..HeadId : history[i].kind = "review"}
Latest(S) == CHOOSE i \in S : \A j \in S : j <= i
Receipt(r) ==
  LET ids == {i \in ReviewIds : history[i].req = r}
  IN IF ids = {} THEN 0 ELSE Latest(ids)

\* A "path" abstracts an independently restorable source group.
\* Every group outside the constant Scope retains the candidate's bytes.
Result(b, c, rs) ==
  [p \in Paths |-> IF p \in rs THEN history[b].tree[p]
                   ELSE history[c].tree[p]]
Changed(b, c, rs) ==
  {p \in rs : history[b].tree[p] # history[c].tree[p]}
Target == IF pending = NoPlan THEN HeadTree
          ELSE Result(pending.base, pending.cand, pending.restore)
ScopedEqual(a, b) == \A p \in Scope : a[p] = b[p]
CaptureTree == [p \in Paths |-> IF p \in Scope THEN work[p]
                              ELSE HeadTree[p]]
SamePlan(entry, b, c, rs, done) ==
  /\ entry.base = b /\ entry.cand = c
  /\ entry.restore = rs /\ entry.complete = done

Init ==
  /\ history = <<Commit(InitialTree, "initial", NoRequest,
                         0, 0, {}, FALSE)>>
  /\ baseline = 1
  /\ work = InitialTree /\ index = InitialTree
  /\ phase = "idle" /\ pending = NoPlan /\ remaining = {}
  /\ repoReady = TRUE /\ reply = NoReply
  /\ unseenOverwrite = FALSE

\* External writers cooperate only while a Margin mutation is active.
\* They may edit while the writer studies a diff, or after a crash.
ExternalAllowed == ~Cooperative \/ phase \in {"idle", "down", "recovery"}
ExternalWrite(t) ==
  /\ ExternalAllowed /\ work' = t
  /\ UNCHANGED <<history, baseline, index, phase, pending,
                  remaining, repoReady, reply, unseenOverwrite>>
ExternalIndex(t) ==
  /\ ExternalAllowed /\ index' = t
  /\ UNCHANGED <<history, baseline, work, phase, pending,
                  remaining, repoReady, reply, unseenOverwrite>>
RepositoryCondition(ok) ==
  /\ ExternalAllowed /\ repoReady' = ok
  /\ UNCHANGED <<history, baseline, work, index, phase, pending,
                  remaining, reply, unseenOverwrite>>

\* A successful, stable draft checkpoint. includeStaged is explicit.
\* Failed Git commands and failed captures do not create this transition.
Checkpoint(includeStaged) ==
  /\ phase = "idle" /\ repoReady /\ HeadId < MaxCommits
  /\ CaptureTree # HeadTree
  /\ IF includeStaged THEN index = CaptureTree ELSE index = HeadTree
  /\ history' = Append(history,
        Commit(CaptureTree, "draft", NoRequest, 0, 0, {}, FALSE))
  /\ index' = CaptureTree
  /\ reply' = NoReply
  /\ UNCHANGED <<baseline, work, phase, pending, remaining,
                  repoReady, unseenOverwrite>>

\* The caller retains/recreates a plan; Margin has no durable plan journal.
\* Receipt lookup precedes stale validation, including on --resume.
Submit(r, b, c, rs, done, mode) ==
  /\ phase = "idle"
  /\ LET oid == Receipt(r)
         expected == IF mode = "resume" THEN Result(b, c, rs)
                     ELSE HeadTree
         err == IF ~done THEN "incomplete"
                ELSE IF ~repoReady THEN "precondition"
                ELSE IF b # baseline \/ c # HeadId
                        \/ ~ScopedEqual(work, expected)
                        \/ index # expected THEN "stale"
                ELSE IF HeadId = MaxCommits THEN "model-bound"
                ELSE "none"
     IN IF oid # 0 THEN
          /\ reply' = Reply(
               IF SamePlan(history[oid], b, c, rs, done)
                 THEN "success" ELSE "request-conflict",
               r, b, c, rs, done, oid)
          /\ UNCHANGED <<phase, pending, remaining>>
        ELSE IF err # "none" THEN
          /\ reply' = Reply(err, r, b, c, rs, done, 0)
          /\ UNCHANGED <<phase, pending, remaining>>
        ELSE
          /\ pending' = Plan(r, b, c, rs, done)
          /\ phase' = IF mode = "resume" \/ Changed(b, c, rs) = {}
                       THEN "commit" ELSE "apply"
          /\ remaining' = IF mode = "resume" THEN {}
                            ELSE Changed(b, c, rs)
          /\ reply' = NoReply
  /\ UNCHANGED <<history, baseline, work, index,
                  repoReady, unseenOverwrite>>

\* Internal, non-atomic worktree effects of the checked --index patch.
ApplyGroup(p) ==
  /\ phase = "apply" /\ p \in remaining
  /\ work' = [work EXCEPT ![p] = Target[p]]
  /\ unseenOverwrite' = (unseenOverwrite \/
       (work[p] # history[pending.cand].tree[p] /\ work[p] # Target[p]))
  /\ remaining' = remaining \ {p}
  /\ phase' = IF remaining' = {} THEN "index" ELSE "apply"
  /\ UNCHANGED <<history, baseline, index, pending, repoReady, reply>>

\* Index publication is part of git apply --index, not git add/restaging.
UpdateIndex ==
  /\ phase = "index" /\ index = history[pending.cand].tree
  /\ index' = Target /\ phase' = "commit"
  /\ UNCHANGED <<history, baseline, work, pending, remaining,
                  repoReady, reply, unseenOverwrite>>

\* Models successful expected-parent publication of an exact Git tree.
\* Review metadata and the idempotency receipt belong to this same commit.
Publish ==
  /\ phase = "commit" /\ repoReady
  /\ HeadId = pending.cand /\ baseline = pending.base
  /\ HeadId < MaxCommits /\ Receipt(pending.req) = 0
  /\ index = Target /\ ScopedEqual(work, Target)
  /\ history' = Append(history,
       Commit(Target, "review", pending.req, pending.base,
              pending.cand, pending.restore, pending.complete))
  /\ baseline' = HeadId + 1 /\ phase' = "respond"
  /\ UNCHANGED <<work, index, pending, remaining,
                  repoReady, reply, unseenOverwrite>>

Respond ==
  /\ phase = "respond"
  /\ reply' = Reply("success", pending.req, pending.base, pending.cand,
                    pending.restore, pending.complete, pending.cand + 1)
  /\ phase' = "idle" /\ pending' = NoPlan /\ remaining' = {}
  /\ UNCHANGED <<history, baseline, work, index, repoReady, unseenOverwrite>>

Fail ==
  /\ phase \in {"apply", "index", "commit"}
  /\ reply' = Reply("failed", pending.req, pending.base, pending.cand,
                    pending.restore, pending.complete, 0)
  /\ phase' = "recovery" /\ pending' = NoPlan /\ remaining' = {}
  /\ UNCHANGED <<history, baseline, work, index, repoReady, unseenOverwrite>>
Crash ==
  /\ phase # "down"
  /\ phase' = "down" /\ pending' = NoPlan
  /\ remaining' = {} /\ reply' = NoReply
  /\ UNCHANGED <<history, baseline, work, index, repoReady, unseenOverwrite>>
Restart ==
  /\ phase = "down" /\ phase' = "recovery"
  /\ UNCHANGED <<history, baseline, work, index, pending, remaining,
                  repoReady, reply, unseenOverwrite>>
InspectRecovery ==
  /\ phase = "recovery" /\ phase' = "idle"
  /\ UNCHANGED <<history, baseline, work, index, pending, remaining,
                  repoReady, reply, unseenOverwrite>>

Advance == (\E p \in Scope : ApplyGroup(p)) \/ UpdateIndex \/ Publish \/ Respond
Commands ==
  (\E staged \in BOOLEAN : Checkpoint(staged)) \/
  (\E r \in RequestIds, b \in 1..HeadId, c \in 1..HeadId,
      rs \in SUBSET Scope, done \in BOOLEAN, mode \in {"apply", "resume"} :
       Submit(r, b, c, rs, done, mode))
Environment == (\E t \in Trees : ExternalWrite(t) \/ ExternalIndex(t))
               \/ (\E ok \in BOOLEAN : RepositoryCondition(ok))
NormalNext == Environment \/ Commands \/ Advance
Next == NormalNext \/ Fail \/ Crash \/ Restart \/ InspectRecovery
Spec == Init /\ [][Next]_vars
\* Conditional liveness: cooperative, failure-free execution plus scheduling.
LiveSpec == Init /\ [][NormalNext]_vars /\ WF_vars(Advance)
Progress == (phase \in Active) ~> (phase = "idle")

CommitType ==
  [tree : Trees, kind : {"initial", "draft", "review"},
   req : RequestIds \cup {NoRequest}, base : 0..MaxCommits,
   cand : 0..MaxCommits, restore : SUBSET Scope, complete : BOOLEAN]
PlanType ==
  [req : RequestIds \cup {NoRequest}, base : 0..MaxCommits,
   cand : 0..MaxCommits, restore : SUBSET Scope, complete : BOOLEAN]
ReplyType ==
  [code : {"none", "success", "request-conflict", "incomplete",
           "precondition", "stale", "model-bound", "failed"},
   req : RequestIds \cup {NoRequest}, base : 0..MaxCommits,
   cand : 0..MaxCommits, restore : SUBSET Scope,
   complete : BOOLEAN, oid : 0..MaxCommits]
TypeOK ==
  /\ history \in Seq(CommitType) /\ HeadId \in 1..MaxCommits
  /\ baseline \in 1..HeadId /\ work \in Trees /\ index \in Trees
  /\ phase \in Phases /\ pending \in PlanType /\ reply \in ReplyType
  /\ remaining \subseteq Scope /\ repoReady \in BOOLEAN
  /\ unseenOverwrite \in BOOLEAN
  /\ (phase \in Active =>
       /\ pending.req \in RequestIds
       /\ pending.base \in 1..HeadId /\ pending.cand \in 1..HeadId)
ApprovalExact ==
  \A i \in ReviewIds :
    /\ history[i].complete /\ history[i].req \in RequestIds
    /\ history[i].base \in 1..(i-1) /\ history[i].cand = i-1
    /\ history[i].base = Latest({1} \cup
         {j \in 1..(i-1) : history[j].kind = "review"})
    /\ history[i].tree =
         Result(history[i].base, history[i].cand, history[i].restore)
BaselineIsReviewed == baseline = Latest({1} \cup ReviewIds)
PinnedWhileApplying ==
  phase \in {"apply", "index", "commit"} =>
    /\ pending.base = baseline /\ pending.cand = HeadId
RequestAtMostOnce ==
  \A i, j \in ReviewIds : history[i].req = history[j].req => i = j
SuccessHasReceipt ==
  reply.code = "success" =>
    /\ reply.oid \in ReviewIds
    /\ history[reply.oid].req = reply.req
    /\ SamePlan(history[reply.oid], reply.base, reply.cand,
                reply.restore, reply.complete)
NoScopeLeak ==
  \A i \in 2..HeadId, p \in Paths \ Scope :
    history[i].tree[p] = history[i-1].tree[p]
NoUnseenOverwrite == ~unseenOverwrite
\* Action properties must use the [A]_v form; [] applies to [A]_vars.
\* A stuttering step leaves vars unchanged and satisfies [A]_vars trivially.
HistoryAppendOnly ==
  [][(/\ Len(history') >= Len(history)
     /\ \A i \in 1..Len(history) : history'[i] = history[i])]_vars
DraftDoesNotApprove ==
  [][((Len(history') > Len(history) /\
       history'[Len(history')].kind = "draft") => baseline' = baseline)]_vars
\* Recovery cannot publish, even a syntactically valid synthetic approval.
\* This property is expressed independently of action definitions.
RecoveryDoesNotPublish ==
  [][((phase \in {"down", "recovery"} \/
        phase' \in {"down", "recovery"}) =>
       /\ history' = history /\ baseline' = baseline)]_vars
=============================================================================
