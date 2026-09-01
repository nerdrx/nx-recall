// Where the transcript draws its separators, as a decision with no DOM in it.
//
// It lives on its own because 0.7.4 gave it a second caller. A full repaint
// walks the whole list; a PREPENDED page walks the new rows and then has to
// hand its state to the seam — the row that used to be first now has a
// predecessor, so the day header it was given may be wrong and the thread
// hairline it was denied may now be right. Two copies of these rules would
// agree everywhere except at that boundary, which is the one place nobody
// looks until they are four pages into their own history.

import { fmtDay } from './dom.js';

/**
 * A separator state machine over one pass down the list, oldest first.
 *
 * Call it with each segment in turn; it answers what belongs ABOVE that row.
 * The state it carries is the previous row's day and the last thread id that
 * was not null, plus whether anything has been emitted yet.
 *
 * The rules, unchanged from 0.6.2 apart from being written down once:
 *  - a new calendar day gets a day header, and resets the thread — a new day
 *    is a new conversation whatever the ids say;
 *  - a thread id that differs from the last non-null one gets a hairline;
 *  - a row with no thread id at all gets nothing and does not reset anything,
 *    because a daemon older than threading never sent one and its transcripts
 *    must render exactly as they always did;
 *  - the FIRST row on screen gets a day header and never a thread hairline:
 *    there is nothing above it for a boundary to be a boundary from.
 */
export function separatorWalker({ day = null, thread = null, seen = false } = {}) {
  return function separatorsFor(seg) {
    const d = fmtDay(seg.t_ms);
    const newDay = d !== day;
    if (newDay) {
      day = d;
      thread = null;
    }
    const newThread = seg.thread != null && seg.thread !== thread && !(thread == null && !seen);
    if (seg.thread != null) thread = seg.thread;
    seen = true;
    return { day: newDay, thread: newThread, threadId: seg.thread };
  };
}

/**
 * The same walk, as a plan over a whole list. What the tests assert against,
 * and what makes "does a prepend seam match a repaint?" a question with a
 * yes-or-no answer rather than a screenshot.
 */
export function separatorPlan(rows, state) {
  const walk = separatorWalker(state);
  return rows.map((seg) => ({ id: seg.id, ...walk(seg) }));
}
