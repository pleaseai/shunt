import { useCallback, useRef, useState } from 'react';

import { API, mutate } from './api';
import { useSession } from './session';

/**
 * How long a completion request may stay in flight before the page stops
 * waiting for it. Generous — it covers an upstream token exchange plus the store
 * write — because its only job is to keep a connection that never settles from
 * closing the Complete button for the life of the page.
 */
const COMPLETE_TIMEOUT_MS = 120_000;

/**
 * Said whenever the completion's outcome is genuinely unknown: the request was
 * abandoned without an answer, or the answer could not be read as one. The code
 * is single-use, so an operator sent to retry an exchange that already stored
 * the account gets a confusing second failure — the table is the authority.
 */
const UNKNOWN_COMPLETION =
  'No answer from the server — the account may still have been stored; recheck the table before retrying';

/**
 * Said when the start request never produced an answer at all. Deliberately not
 * `copy.startFailure`, which reports an answer the server *gave*: that one means
 * the login was refused, this one that the request may never have arrived. The
 * same line is already drawn on the completion path, and for the same reason —
 * telling an operator their login was refused, when nothing is known to have
 * reached the server, sends them to fix a request that was never read.
 *
 * Shared by both forms rather than added to `copy`, matching `UNKNOWN_COMPLETION`:
 * neither says anything a caller could usefully word differently, because
 * neither knows what happened.
 */
const START_UNANSWERED = 'No answer from the server — no authorization step opened, so start again';

/**
 * `START_UNANSWERED` for the case where this start also closed a step. Spelled
 * out rather than composed from the two constants: concatenating them repeats
 * "start again" and stacks a second em dash on a sentence that already has one.
 * The two paths are symmetric in what they cost the operator — the clear that
 * closes the previous step runs before the request, so it has happened either
 * way by the time this is read (#531).
 */
const START_UNANSWERED_AFTER_CLOSE =
  'No answer from the server — no authorization step opened, and the previous one was closed when this start was issued, so start again';

/**
 * Appended to a rejected start's own error when an authorization step the
 * operator could still have completed was closed — by this start, or by an
 * earlier one whose own response never got to report it. The clear itself
 * is by design — a step left open stays clickable and its Complete button posts
 * to the name captured for THAT flow (#513) — but the server holds the pending
 * login for the rest of its `pending_ttl_secs`, and after a rejection nothing on
 * the page points at it any more. Without this line the operator reads only why
 * the new request was refused and goes looking for a link that is gone; with it
 * they know the step is closed and restart (#531).
 *
 * Said only when a step was in fact closed. A rejected start that closed
 * nothing has nothing to report, and saying it regardless would teach an
 * operator to read past the sentence on the one occasion it is true.
 */
const CLOSED_PREVIOUS_STEP =
  'the previous authorization step was closed when this start was issued; start again.';

export interface FlowMessage {
  text: string;
  ok: boolean;
}

export interface Flow {
  name: string;
  setName: (value: string) => void;
  code: string;
  setCode: (value: string) => void;
  /** Non-null exactly while the authorization step is open. */
  authorizeUrl: string | null;
  /** True while a start request is in flight, i.e. after the step closed but
   *  before the next one opens. */
  starting: boolean;
  message: FlowMessage | null;
  completing: boolean;
  start: (body: Record<string, unknown>) => Promise<void>;
  complete: () => Promise<void>;
  /** Point the form at `name` and discard any half-finished flow. */
  prime: (name: string) => void;
  report: (text: string, ok: boolean) => void;
}

/**
 * One account-provisioning form's flow: start, authorize, complete.
 *
 * The epoch is what keeps a superseded request from writing its result back. A
 * start or completion still in flight when the form is re-primed would otherwise
 * restore the flow `prime` just cleared, leaving the name field naming one
 * account while `currentName` — the handle the completion POST interpolates into
 * its URL — still names the previous one. Following that reopened link then
 * stores the newly authorized credential under the OLD account's name, silently
 * overwriting a different pool account. Each request captures the epoch it was
 * issued under and discards its own response once a newer start or a re-login
 * has superseded it.
 *
 * Each form gets its own hook instance, so the two counters are separate by
 * construction: re-priming one never discards the other's live flow.
 */
export function useProvisioningFlow({
  endpoints,
  copy,
  onStored,
}: {
  endpoints: { start: string; complete: (name: string) => string };
  copy: { startFailure: string; completeFailure: string; stored: string };
  /** Re-read every table the completed store write can have changed. */
  onStored: () => void;
}): Flow {
  const { csrf } = useSession();
  const [name, setName] = useState('');
  const [code, setCode] = useState('');
  const [authorizeUrl, setAuthorizeUrl] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [message, setMessage] = useState<FlowMessage | null>(null);
  const [completing, setCompleting] = useState(false);

  const epoch = useRef(0);
  const currentName = useRef<string | null>(null);
  // A completion is the one request in this flow that consumes the pending
  // login, so unlike a start it must not be superseded by a second click of its
  // own button. The first completion is the one that stores the credential; a
  // second finds the pending entry already consumed and fails. Letting that
  // second click bump the epoch would silence the successful response — the
  // confirmation and the form reset are gated on the epoch — and surface the
  // failed one instead, reporting an error over an account that was in fact
  // stored and leaving the finished form open. The button is closed for the
  // duration of its own request instead. This reaches one page's own two clicks
  // and no further: a reload clears it and permits the same retry, and a second
  // tab or a direct API call never sees it. Ordering concurrent completions is
  // server-side work (issue #440).
  const completingNow = useRef(false);
  // A step this form closed whose closure no message has reported yet. `start`
  // closes the open step synchronously, before it sends, but its own response
  // can be dropped by the epoch guard when a newer start supersedes it — and
  // that newer start reads an `authorizeUrl` the older one already nulled. The
  // ref is what carries the fact across that gap, so the closure is reported by
  // whichever message does get through instead of by nobody. Cleared as soon as
  // something reports it, and as soon as a step is open again.
  const closedStepUnreported = useRef(false);

  const report = useCallback((text: string, ok: boolean) => setMessage({ text, ok }), []);

  const prime = useCallback((next: string) => {
    setName(next);
    epoch.current += 1;
    currentName.current = null;
    closedStepUnreported.current = false;
    setAuthorizeUrl(null);
    setStarting(false);
    setCode('');
    setMessage(null);
  }, []);

  const start = useCallback(
    async (body: Record<string, unknown>) => {
      setMessage(null);
      // Whether this start closes an authorization step the operator could
      // still have completed — the fact the notices below report. Captured
      // before the clear a few lines down, and from two sources, because the
      // closure and the message that reports it can be separated: `authorizeUrl`
      // is the step being closed right now, and `closedStepUnreported` is one an
      // earlier start closed whose own response the epoch guard then dropped.
      //
      // Deliberately not `starting || authorizeUrl !== null`, the predicate
      // `AddClaudeAccount`'s radio lock uses (the Codex form has no radios and
      // no such lock). That one reaches the superseded case too, but it fires
      // just as readily on two chained starts that never opened a step at all,
      // and then names a step the operator never saw. The ref reaches the same
      // case by remembering an actual closure, so it cannot say that.
      const closedOpenStep = authorizeUrl !== null || closedStepUnreported.current;
      closedStepUnreported.current = closedOpenStep;
      // The previous flow's authorization step is closed the moment a new start
      // is issued. Left open it stays clickable, and its Complete button posts
      // to the name captured for THAT flow — and because `complete` bumps the
      // epoch itself, that click also strands the start now in flight: its
      // response arrives under a superseded epoch and is dropped, so the link
      // the operator is looking at is never replaced by the one they asked for.
      setAuthorizeUrl(null);
      currentName.current = null;
      // The code belongs to the flow being closed. `complete` clears it only on
      // success, so a failed exchange leaves it in the box, and it would be
      // submitted against the new pending entry — which fails on a state
      // mismatch, blaming the operator's fresh paste for a stale one.
      setCode('');
      // `authorizeUrl` alone cannot carry the login-method lock: it is null from
      // here until the response lands, so the radios would reopen for exactly as
      // long as the request that already captured `mode` is in flight. Released
      // only by the epoch's owner — a superseded start leaves it to the newer
      // start or to `prime`, either of which sets it as it takes over.
      setStarting(true);
      const issued = (epoch.current += 1);
      try {
        const result = await mutate(`${API}${endpoints.start}`, csrf, {
          method: 'POST',
          body: JSON.stringify(body),
        });
        if (issued !== epoch.current) return;
        setStarting(false);
        // `!answered` is a failure too: without a readable `authorize_url` the
        // form has nothing to show, so reporting nothing would leave the
        // operator staring at a step that never opened.
        if (!result.ok || !result.answered) {
          const reason = result.message ?? copy.startFailure;
          setMessage({
            text: closedOpenStep ? `${reason} — ${CLOSED_PREVIOUS_STEP}` : reason,
            ok: false,
          });
          closedStepUnreported.current = false;
          return;
        }
        currentName.current = (result.payload.name as string | undefined) ?? null;
        const opened = (result.payload.authorize_url as string | undefined) ?? null;
        setAuthorizeUrl(opened);
        // Only when a step is actually open again. A 2xx answer carrying no
        // `authorize_url` lands *here*, not in the branch above — that one reads
        // the status and whether the body parsed, never the payload — and it
        // opened nothing, so the closure it caused stays unreported for the next
        // message to carry.
        if (opened !== null) closedStepUnreported.current = false;
      } catch {
        if (issued === epoch.current) {
          setStarting(false);
          setMessage({
            text: closedOpenStep ? START_UNANSWERED_AFTER_CLOSE : START_UNANSWERED,
            ok: false,
          });
          closedStepUnreported.current = false;
        }
      }
    },
    [csrf, endpoints, copy.startFailure, authorizeUrl],
  );

  const complete = useCallback(async () => {
    if (completingNow.current) return;
    completingNow.current = true;
    setCompleting(true);
    const issued = (epoch.current += 1);
    // A completion that never settles would strand the marker above and close
    // the button for the life of the page. Releasing the marker on a newer start
    // is not the way out: that would put two completions in flight against
    // different pending entries, and the server does not order them —
    // `PendingStore::attempt` leaves the entry in place and `complete_account`
    // removes it only after the store, so the older exchange can land last and
    // leave the account holding the superseded credential. The request itself is
    // bounded instead.
    const abort = new AbortController();
    const bound = setTimeout(() => abort.abort(), COMPLETE_TIMEOUT_MS);
    try {
      const result = await mutate(
        `${API}${endpoints.complete(encodeURIComponent(currentName.current ?? ''))}`,
        csrf,
        { method: 'POST', body: JSON.stringify({ code: code.trim() }), signal: abort.signal },
      );
      // The answer could not be read as JSON, and every admin mutation sends
      // JSON — so it came from something else (a proxy's error page, a
      // truncated body) and says nothing about whether the code was exchanged.
      // That is the same unknown the abandoned-request path below reports, and
      // it is reported the same way rather than as the definite failure `ok`
      // alone would make it. `src/admin/script.rs` draws the same line: its
      // completion handler's `await res.json()` is bare so an unreadable answer
      // reaches this path, while remove and refresh use `.catch(() => ({}))`.
      if (!result.answered) {
        onStored();
        if (issued === epoch.current) setMessage({ text: UNKNOWN_COMPLETION, ok: false });
        return;
      }
      if (!result.ok) {
        if (issued === epoch.current) {
          setMessage({ text: result.message ?? copy.completeFailure, ok: false });
        }
        return;
      }
      // The account was stored upstream whether or not this flow has since been
      // superseded, so the tables must refresh either way — they re-read the
      // server and touch no flow-local state. Only the confirmation and the form
      // reset stay gated: those would stomp the newly primed flow.
      onStored();
      if (issued !== epoch.current) return;
      setMessage({ text: (result.payload.message as string | undefined) ?? copy.stored, ok: true });
      setAuthorizeUrl(null);
      setName('');
      setCode('');
    } catch {
      // The request was abandoned without an answer — it timed out, hit the
      // bound above, or the connection failed — so from here it is unknown
      // whether the account was stored. Refresh the tables and say so. The
      // refresh races an exchange that may still be running, so it settles the
      // question only if the store has already landed; that is worth one request
      // and no more. Polling for a completion that has already missed a
      // two-minute deadline would relocate the same ambiguity to a later one,
      // and reading it authoritatively needs a server-side completion status
      // this surface does not have (issue #440).
      onStored();
      if (issued === epoch.current) {
        setMessage({ text: UNKNOWN_COMPLETION, ok: false });
      }
    } finally {
      clearTimeout(bound);
      completingNow.current = false;
      setCompleting(false);
    }
  }, [csrf, code, endpoints, copy.completeFailure, copy.stored, onStored]);

  return {
    name,
    setName,
    code,
    setCode,
    authorizeUrl,
    starting,
    message,
    completing,
    start,
    complete,
    prime,
    report,
  };
}
