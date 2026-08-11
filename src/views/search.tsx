import { useEffect, useRef, useState } from "preact/hooks";
import {
  listPeople,
  search,
  type Person,
  type SearchFilters,
  type SearchHit,
} from "../lib/api";
import { fmtDate } from "./meetings";
import { notifyError } from "../lib/notify";

const PAGE = 40;

// Survives unmount so Back after opening a hit keeps the same filters.
let lastFilters = { who: "", dateFrom: "", dateTo: "" };

function fmtTs(ms: number): string {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

/**
 * Backend snippets delimit matches with \x01…\x02 (not HTML) so transcript
 * text can never inject markup; render the delimited runs as <mark>.
 */
function Snippet({ text }: { text: string }) {
  const parts = [];
  let rest = text;
  for (;;) {
    const a = rest.indexOf("\x01");
    if (a < 0) break;
    const b = rest.indexOf("\x02", a + 1);
    if (b < 0) break;
    if (a > 0) parts.push(<span key={`text-${parts.length}`}>{rest.slice(0, a)}</span>);
    parts.push(<mark key={`mark-${parts.length}`}>{rest.slice(a + 1, b)}</mark>);
    rest = rest.slice(b + 1);
  }
  if (rest) parts.push(<span key={`text-${parts.length}`}>{rest}</span>);
  return <span>{parts}</span>;
}

export function SearchView(props: {
  query: string;
  onOpen: (meetingId: number, segmentId?: number) => void;
}) {
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [people, setPeople] = useState<Person[]>([]);
  // "" = anyone, "me"/"them" = track filter, "p<id>" = person filter.
  const [who, setWho] = useState(lastFilters.who);
  const [dateFrom, setDateFrom] = useState(lastFilters.dateFrom);
  const [dateTo, setDateTo] = useState(lastFilters.dateTo);
  const [loading, setLoading] = useState(false);
  const requestGeneration = useRef(0);

  const filters = (): SearchFilters => ({
    personId: who.startsWith("p") ? parseInt(who.slice(1), 10) : null,
    track: who === "me" ? "mic" : who === "them" ? "loopback" : null,
    dateFrom: dateFrom || null,
    dateTo: dateTo || null,
  });

  const load = (offset: number, append: boolean) => {
    const generation = ++requestGeneration.current;
    setLoading(true);
    return search(props.query, offset, filters())
      .then((rows) => {
        if (generation !== requestGeneration.current) return;
        const transcriptHits = rows.filter((row) => row.segment_id > 0).length;
        setError(null);
        setHasMore(transcriptHits >= PAGE);
        setHits((prev) => (append ? [...prev, ...rows] : rows));
      })
      .catch((e) => {
        if (generation === requestGeneration.current) {
          setError(String(e));
          notifyError("Could not search transcripts", e);
        }
      })
      .finally(() => {
        if (generation === requestGeneration.current) setLoading(false);
      });
  };

  useEffect(() => {
    let cancelled = false;
    listPeople()
      .then((rows) => {
        if (!cancelled) setPeople(rows);
      })
      .catch((error) => {
        if (!cancelled) notifyError("Could not load people for search filters", error);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    lastFilters = { who, dateFrom, dateTo };
  }, [who, dateFrom, dateTo]);

  useEffect(() => {
    // Drop the previous query's results so they can't render under the
    // new query's heading while the search is in flight.
    setHits([]);
    setHasMore(false);
    void load(0, false);
    return () => {
      requestGeneration.current += 1;
    };
  }, [props.query, who, dateFrom, dateTo]);

  // Group consecutive hits by meeting (results come ordered by relevance,
  // so group by meeting id over the whole set instead).
  const groups = new Map<number, SearchHit[]>();
  for (const h of hits) {
    const g = groups.get(h.meeting_id);
    if (g) g.push(h);
    else groups.set(h.meeting_id, [h]);
  }

  return (
    <div class="search-view">
      <h2 class="search-heading" aria-live="polite">
        Results for “{props.query}”
        <span class="muted"> — {hits.length}{hasMore ? "+" : ""} match{hits.length === 1 ? "" : "es"}</span>
      </h2>
      <div class="search-filters">
        <select
          class="player-speed"
          title="Who said it"
          aria-label="Filter by speaker"
          value={who}
          onChange={(e) => setWho((e.target as HTMLSelectElement).value)}
        >
          <option value="">Anyone</option>
          <option value="me">Me</option>
          <option value="them">Them</option>
          {people.map((p) => (
            <option value={`p${p.id}`} key={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <label class="muted">
          from{" "}
          <input
            type="date"
            class="date-input"
            value={dateFrom}
            onChange={(e) => setDateFrom((e.target as HTMLInputElement).value)}
          />
        </label>
        <label class="muted">
          to{" "}
          <input
            type="date"
            class="date-input"
            value={dateTo}
            onChange={(e) => setDateTo((e.target as HTMLInputElement).value)}
          />
        </label>
        {(who || dateFrom || dateTo) && (
          <button
            type="button"
            class="btn btn-ghost"
            onClick={() => {
              setWho("");
              setDateFrom("");
              setDateTo("");
            }}
          >
            Clear filters
          </button>
        )}
      </div>
      {error && (
        <div class="view-error" role="alert">
          <p>Search failed: {error}</p>
          <button type="button" class="btn" onClick={() => void load(0, false)}>
            Try again
          </button>
        </div>
      )}
      {loading && hits.length === 0 && !error && (
        <div class="empty muted" role="status">Searching…</div>
      )}
      {!loading && !error && hits.length === 0 && (
        <div class="empty muted">No matches.</div>
      )}
      {!error && [...groups.entries()].map(([meetingId, group]) => (
        <div class="search-group" key={meetingId}>
          <h3 class="search-group-title">
            {group[0].meeting_title}
            <span class="muted"> · {fmtDate(group[0].started_at)}</span>
          </h3>
          {group.map((h) => (
            <button
              type="button"
              class="search-hit"
              key={h.segment_id}
              onClick={() =>
                // Notes hits (segment_id -1) open the meeting without a seek.
                props.onOpen(h.meeting_id, h.segment_id > 0 ? h.segment_id : undefined)
              }
            >
              {h.segment_id > 0 ? (
                <span class="ts">[{fmtTs(h.start_ms)}]</span>
              ) : (
                <span class="chip chip-bookmark">note</span>
              )}{" "}
              {h.speaker_name && <span class="muted">{h.speaker_name}: </span>}
              <Snippet text={h.snippet} />
            </button>
          ))}
        </div>
      ))}
      {!error && hasMore && (
        <button
          type="button"
          class="btn btn-ghost"
          disabled={loading}
          onClick={() =>
            void load(hits.filter((hit) => hit.segment_id > 0).length, true)
          }
        >
          {loading ? "Loading…" : "Load more"}
        </button>
      )}
    </div>
  );
}
