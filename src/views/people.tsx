import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import {
  deletePerson,
  getPeopleStats,
  listMeetings,
  listPeople,
  type Meeting,
  type Person,
  type PersonMeetingStat,
} from "../lib/api";
import { fmtDate, fmtDuration } from "./meetings";
import { appConfirm } from "../lib/confirm";
import { notifyError } from "../lib/notify";

const WEEKS = 12;

/** Monday of the week containing `d`, at midnight local. */
function weekStart(d: Date): Date {
  const out = new Date(d);
  out.setHours(0, 0, 0, 0);
  out.setDate(out.getDate() - ((out.getDay() + 6) % 7));
  return out;
}

function WeeklyTrend({ meetings }: { meetings: Meeting[] }) {
  const weeks = useMemo(() => {
    const start = weekStart(new Date());
    const buckets = Array.from({ length: WEEKS }, (_, i) => {
      const from = new Date(start);
      from.setDate(from.getDate() - 7 * (WEEKS - 1 - i));
      return { from, count: 0, ms: 0 };
    });
    for (const m of meetings) {
      const ws = weekStart(new Date(m.started_at)).getTime();
      const bucket = buckets.find((b) => b.from.getTime() === ws);
      if (bucket) {
        bucket.count += 1;
        bucket.ms += m.duration_ms ?? 0;
      }
    }
    return buckets;
  }, [meetings]);

  const maxMs = Math.max(1, ...weeks.map((w) => w.ms));
  const barW = 100 / WEEKS;
  return (
    <div class="person-card trend-card">
      <div class="trend-title muted">
        Last {WEEKS} weeks —{" "}
        {weeks.reduce((s, w) => s + w.count, 0)} meetings,{" "}
        {fmtDuration(weeks.reduce((s, w) => s + w.ms, 0))} total
      </div>
      <svg
        viewBox="0 0 100 34"
        preserveAspectRatio="none"
        class="trend-chart"
        aria-hidden="true"
      >
        {weeks.map((w, i) => {
          const h = w.ms > 0 ? Math.max(2, (w.ms / maxMs) * 30) : 0.8;
          return (
            <rect
              key={w.from.getTime()}
              x={i * barW + barW * 0.15}
              y={32 - h}
              width={barW * 0.7}
              height={h}
              rx={0.8}
              class={w.ms > 0 ? "trend-bar" : "trend-bar trend-bar-empty"}
            >
              <title>
                {`Week of ${w.from.toLocaleDateString()}: ${w.count} meeting${w.count === 1 ? "" : "s"}, ${fmtDuration(w.ms)}`}
              </title>
            </rect>
          );
        })}
      </svg>
      <ul class="sr-only">
        {weeks.map((w) => (
          <li key={w.from.getTime()}>
            Week of {w.from.toLocaleDateString()}: {w.count} meeting
            {w.count === 1 ? "" : "s"}, {fmtDuration(w.ms)}
          </li>
        ))}
      </ul>
    </div>
  );
}

interface PersonAgg {
  key: string;
  personId: number | null;
  name: string;
  isMe: boolean;
  meetings: PersonMeetingStat[];
  meetingCount: number;
  togetherMs: number; // total duration of meetings shared with them
  talkMs: number;
  words: number;
}

export function PeopleView(props: {
  refreshTick: number;
  onOpen: (meetingId: number) => void;
}) {
  const [stats, setStats] = useState<PersonMeetingStat[]>([]);
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [prints, setPrints] = useState<Person[]>([]);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const requestGeneration = useRef(0);

  const load = async () => {
    const generation = ++requestGeneration.current;
    setLoading(true);
    try {
      const loadAllMeetings = async () => {
        const all: Meeting[] = [];
        for (;;) {
          const page = await listMeetings(all.length, 500);
          all.push(...page);
          if (page.length < 500) return all;
        }
      };
      const [nextStats, nextMeetings, nextPeople] = await Promise.all([
        getPeopleStats(),
        loadAllMeetings(),
        listPeople(),
      ]);
      if (generation !== requestGeneration.current) return;
      setStats(nextStats);
      setMeetings(nextMeetings);
      setPrints(nextPeople);
      setLoadError(null);
    } catch (error) {
      if (generation !== requestGeneration.current) return;
      setLoadError(String(error));
      notifyError("Could not load people statistics", error);
    } finally {
      if (generation === requestGeneration.current) setLoading(false);
    }
  };

  useEffect(() => {
    void load();
    return () => {
      requestGeneration.current += 1;
    };
  }, [props.refreshTick]);

  const people = useMemo(() => {
    const byPerson = new Map<string, PersonAgg>();
    for (const row of stats) {
      const key = row.person_id != null ? `p${row.person_id}` : "me";
      const agg = byPerson.get(key) ?? {
        key,
        personId: row.person_id,
        name: row.person_name,
        isMe: row.person_id == null,
        meetings: [],
        meetingCount: 0,
        togetherMs: 0,
        talkMs: 0,
        words: 0,
      };
      agg.meetings.push(row);
      agg.meetingCount += 1;
      agg.togetherMs += row.duration_ms;
      agg.talkMs += row.talk_ms;
      agg.words += row.words;
      byPerson.set(key, agg);
    }
    // Me first, then by time spent together.
    return [...byPerson.values()].sort((a, b) => {
      if (a.isMe !== b.isMe) return a.isMe ? -1 : 1;
      return b.togetherMs - a.togetherMs;
    });
  }, [stats]);

  if (loading && people.length === 0) {
    return <div class="empty" role="status">Loading people…</div>;
  }

  if (loadError && people.length === 0) {
    return (
      <div class="view-error" role="alert">
        <p>Could not load people: {loadError}</p>
        <button type="button" class="btn" onClick={() => void load()}>
          Try again
        </button>
      </div>
    );
  }

  if (people.length === 0) {
    return (
      <div class="empty">
        <p>No people yet.</p>
        <p class="muted">
          Stats appear once meetings are transcribed. Rename speakers in a
          transcript to track individual people across meetings.
        </p>
      </div>
    );
  }

  return (
    <div class="people-view">
      <h2 class="sr-only">People</h2>
      <WeeklyTrend meetings={meetings} />
      {people.map((p) => {
        const pid = p.personId;
        const print = pid != null ? prints.find((x) => x.id === pid) : undefined;
        return (
          <div class="person-card" key={p.key}>
            <button
              type="button"
              class="person-head"
              aria-expanded={expanded === p.key}
              aria-controls={`person-meetings-${p.key}`}
              onClick={() => setExpanded(expanded === p.key ? null : p.key)}
            >
              <span class={`chip ${p.isMe ? "chip-me" : "chip-s1"}`} title={p.name}>{p.name}</span>
              <span class="person-stats">
                {p.meetingCount} meeting{p.meetingCount === 1 ? "" : "s"}
                {!p.isMe && <> · {fmtDuration(p.togetherMs)} together</>}
                {" · "}
                {fmtDuration(p.talkMs)} speaking · {p.words.toLocaleString()} words
                {p.togetherMs > 0 && (
                  <> · {Math.round((p.talkMs / Math.max(1, p.togetherMs)) * 100)}% of meeting time</>
                )}
                {print && (
                  <> · {Math.round(print.sample_seconds / 60)} min of speech learned</>
                )}
              </span>
              <span class="muted" aria-hidden="true">{expanded === p.key ? "▾" : "▸"}</span>
            </button>
            {expanded === p.key && (
              <div class="person-meetings" id={`person-meetings-${p.key}`}>
                {p.meetings.map((m) => (
                  <button
                    type="button"
                    class="person-meeting-row"
                    key={m.meeting_id}
                    onClick={() => props.onOpen(m.meeting_id)}
                  >
                    <span class="meeting-title">{m.meeting_title}</span>
                    <span class="meeting-meta">
                      {fmtDate(m.started_at)} · {fmtDuration(m.duration_ms)} meeting ·{" "}
                      {fmtDuration(m.talk_ms)} speaking · {m.words.toLocaleString()} words
                      {m.duration_ms > 0 && (
                        <> · {Math.round((m.talk_ms / m.duration_ms) * 100)}%</>
                      )}
                    </span>
                  </button>
                ))}
                {!p.isMe && pid != null && (
                  <button
                    type="button"
                    class="btn btn-ghost"
                    title="Forget this voice — Shift-click to skip confirmation"
                    onClick={async (e) => {
                      if (
                        !e.shiftKey &&
                        !(await appConfirm(`Forget ${p.name}'s voice?\nExisting transcripts keep their names.`, "Forget voice"))
                      )
                        return;
                      deletePerson(pid)
                        .then(() => void load())
                        .catch((error) => notifyError(`Could not forget ${p.name}`, error));
                    }}
                  >
                    Forget voice…
                  </button>
                )}
              </div>
            )}
          </div>
        );
      })}
      <p class="muted people-hint">
        "Speaking" totals come from transcribed segments; word counts are
        approximate. Unnamed speakers (Speaker 1…) aren't tracked across
        meetings — rename them to a person to include them here. Enrolled
        voices can also be managed in Settings.
      </p>
    </div>
  );
}
