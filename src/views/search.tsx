import { useEffect, useState } from "preact/hooks";
import {
  listPeople,
  search,
  type Person,
  type SearchFilters,
  type SearchHit,
} from "../lib/api";
import { fmtDate } from "./meetings";

const PAGE = 40;

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
    if (a > 0) parts.push(<span>{rest.slice(0, a)}</span>);
    parts.push(<mark>{rest.slice(a + 1, b)}</mark>);
    rest = rest.slice(b + 1);
  }
  if (rest) parts.push(<span>{rest}</span>);
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
  const [who, setWho] = useState("");
  const [dateFrom, setDateFrom] = useState("");
  const [dateTo, setDateTo] = useState("");

  const filters = (): SearchFilters => ({
    personId: who.startsWith("p") ? parseInt(who.slice(1), 10) : null,
    track: who === "me" ? "mic" : who === "them" ? "loopback" : null,
    dateFrom: dateFrom || null,
    dateTo: dateTo || null,
  });

  const load = (offset: number, append: boolean) =>
    search(props.query, offset, filters())
      .then((rows) => {
        setError(null);
        setHasMore(rows.length >= PAGE);
        setHits((prev) => (append ? [...prev, ...rows] : rows));
      })
      .catch((e) => setError(String(e)));

  useEffect(() => {
    listPeople().then(setPeople).catch(() => {});
  }, []);

  useEffect(() => {
    load(0, false);
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
      <h2 class="search-heading">
        Results for “{props.query}”
        <span class="muted"> — {hits.length}{hasMore ? "+" : ""} match{hits.length === 1 ? "" : "es"}</span>
      </h2>
      <div class="search-filters">
        <select
          class="player-speed"
          title="Who said it"
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
      {error && <div class="error">{error}</div>}
      {!error && hits.length === 0 && <div class="empty muted">No matches.</div>}
      {[...groups.entries()].map(([meetingId, group]) => (
        <div class="search-group" key={meetingId}>
          <div class="search-group-title">
            {group[0].meeting_title}
            <span class="muted"> · {fmtDate(group[0].started_at)}</span>
          </div>
          {group.map((h) => (
            <div
              class="search-hit"
              key={h.segment_id}
              onClick={() =>
                // Notes hits (segment_id -1) open the meeting without a seek.
                props.onOpen(h.meeting_id, h.segment_id > 0 ? h.segment_id : undefined)
              }
            >
              {h.speaker_name && <span class="muted">{h.speaker_name}: </span>}
              <Snippet text={h.snippet} />
            </div>
          ))}
        </div>
      ))}
      {hasMore && (
        <button class="btn btn-ghost" onClick={() => load(hits.length, true)}>
          Load more
        </button>
      )}
    </div>
  );
}
