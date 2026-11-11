//! Archive-wide insights for the Stats page: who you meet most, how you
//! and the others talk (filler words, pace, questions), the words each side
//! uses and when meetings happen. Pure aggregation over rows the database
//! streams ([`crate::db::Db::for_each_stats_segment`]); nothing is stored.
//!
//! Me = the mic track; everyone else = the loopback track. Words are
//! counted by [`words`], so totals here can differ a little from People's
//! space-counted ones.

use crate::db::{Meeting, PersonMeetingStat, StatsSegmentRow};
use chrono::{DateTime, Datelike, Local, Timelike};
use std::collections::HashMap;

/// Phrases counted under "Filler words" (every use, so "like" also counts
/// "I'd like to"). Lower case; at most two words.
pub const FILLERS: [&str; 11] = [
    "like",
    "so",
    "actually",
    "basically",
    "literally",
    "you know",
    "i mean",
    "kind of",
    "sort of",
    "um",
    "uh",
];

/// Words in each word cloud.
pub const CLOUD_WORDS: usize = 60;

/// Two segments of one speaker further apart than this are two turns.
const MONOLOGUE_GAP_MS: i64 = 3_000;

/// Common words kept out of the word clouds (with the fillers).
const STOPWORDS: &[&str] = &[
    "about",
    "above",
    "after",
    "again",
    "all",
    "also",
    "and",
    "any",
    "are",
    "aren't",
    "around",
    "back",
    "because",
    "been",
    "before",
    "being",
    "but",
    "can",
    "can't",
    "could",
    "couldn't",
    "did",
    "didn't",
    "does",
    "doesn't",
    "doing",
    "don't",
    "down",
    "each",
    "even",
    "every",
    "few",
    "for",
    "from",
    "get",
    "gets",
    "getting",
    "go",
    "goes",
    "going",
    "gonna",
    "got",
    "gotta",
    "had",
    "has",
    "hasn't",
    "have",
    "haven't",
    "having",
    "he'd",
    "he'll",
    "he's",
    "her",
    "here",
    "here's",
    "hers",
    "him",
    "his",
    "how",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "into",
    "isn't",
    "it'd",
    "it'll",
    "it's",
    "its",
    "just",
    "know",
    "let",
    "let's",
    "like",
    "maybe",
    "mean",
    "more",
    "most",
    "much",
    "must",
    "need",
    "not",
    "now",
    "off",
    "okay",
    "once",
    "one",
    "only",
    "other",
    "our",
    "ours",
    "out",
    "over",
    "own",
    "pretty",
    "probably",
    "put",
    "quite",
    "really",
    "right",
    "said",
    "same",
    "say",
    "says",
    "see",
    "she",
    "she'd",
    "she'll",
    "she's",
    "should",
    "shouldn't",
    "some",
    "something",
    "still",
    "such",
    "sure",
    "take",
    "than",
    "that",
    "that's",
    "the",
    "their",
    "theirs",
    "them",
    "then",
    "there",
    "there's",
    "these",
    "they",
    "they'd",
    "they'll",
    "they're",
    "they've",
    "thing",
    "things",
    "think",
    "this",
    "those",
    "through",
    "too",
    "under",
    "until",
    "very",
    "want",
    "was",
    "wasn't",
    "way",
    "we'd",
    "we'll",
    "we're",
    "we've",
    "well",
    "were",
    "weren't",
    "what",
    "what's",
    "when",
    "where",
    "which",
    "while",
    "who",
    "who's",
    "whom",
    "why",
    "will",
    "with",
    "won't",
    "would",
    "wouldn't",
    "yeah",
    "yes",
    "yep",
    "you",
    "you'd",
    "you'll",
    "you're",
    "you've",
    "your",
    "yours",
    "actually",
    "basically",
    "literally",
    "kind",
    "sort",
    "lot",
    "bit",
    "oh",
    "hmm",
    "mhm",
    "uh-huh",
    "thank",
    "thanks",
    "wanna",
    "kinda",
    "sorta",
];

/// One side's totals (Me, or everyone else).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Voice {
    pub words: u64,
    pub talk_ms: i64,
    /// Question marks in their transcript lines.
    pub questions: u64,
}

impl Voice {
    /// Words per minute of speech, once there is a minute to go on.
    pub fn wpm(&self) -> Option<f64> {
        (self.talk_ms >= 60_000).then(|| self.words as f64 / (self.talk_ms as f64 / 60_000.0))
    }
}

/// A person you have met, over every meeting shared with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonTotal {
    pub person_id: i64,
    pub name: String,
    pub meetings: usize,
    /// Total duration of the meetings shared with them.
    pub together_ms: i64,
    pub talk_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillerCount {
    pub phrase: &'static str,
    pub me: u64,
    pub others: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordCount {
    pub word: String,
    pub count: u64,
}

/// A record holder: a meeting and how long something in it lasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub meeting_id: i64,
    pub title: String,
    /// The speaker, for a monologue.
    pub speaker: Option<String>,
    pub ms: i64,
}

/// Everything the Stats page shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatsReport {
    pub meetings: usize,
    pub recorded_ms: i64,
    pub me: Voice,
    pub others: Voice,
    /// Named people (not Me), most time together first.
    pub people: Vec<PersonTotal>,
    /// [`FILLERS`] in order.
    pub fillers: Vec<FillerCount>,
    /// Most used first; at most [`CLOUD_WORDS`].
    pub my_words: Vec<WordCount>,
    pub their_words: Vec<WordCount>,
    pub longest_meeting: Option<Record>,
    pub longest_monologue: Option<Record>,
    /// Meetings started per local weekday (Monday first) and hour.
    pub week_hours: [[u32; 24]; 7],
}

/// The words of a transcript line: lower case, split on anything but
/// letters, digits and apostrophes (curly ones straightened), edge
/// apostrophes trimmed.
pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '\u{2019}'))
        .map(|w| w.trim_matches(|c| c == '\'' || c == '\u{2019}'))
        .filter(|w| !w.is_empty())
        .map(|w| w.replace('\u{2019}', "'").to_lowercase())
}

/// Whether a word belongs in a word cloud.
fn cloud_worthy(word: &str) -> bool {
    word.chars().count() >= 3
        && !word.chars().all(|c| c.is_ascii_digit())
        && !STOPWORDS.contains(&word)
}

/// The `n` most used words, ties alphabetical (stable across reloads).
fn top_words(counts: HashMap<String, u64>, n: usize) -> Vec<WordCount> {
    let mut words: Vec<WordCount> = counts
        .into_iter()
        .map(|(word, count)| WordCount { word, count })
        .collect();
    words.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
    words.truncate(n);
    words
}

/// Builds a [`StatsReport`] from segments fed in transcript order
/// (meeting, then start time).
pub struct StatsBuilder {
    report: StatsReport,
    my_counts: HashMap<String, u64>,
    their_counts: HashMap<String, u64>,
    /// The speaker turn being followed (for the longest monologue).
    turn: Option<Turn>,
}

struct Turn {
    meeting_id: i64,
    speaker: Option<i64>,
    title: String,
    name: String,
    start_ms: i64,
    end_ms: i64,
}

impl StatsBuilder {
    /// Start from the meetings (count, durations, the week grid) and the
    /// People stats rows (who you meet).
    pub fn new(meetings: &[Meeting], people: &[PersonMeetingStat]) -> StatsBuilder {
        let mut report = StatsReport {
            meetings: meetings.len(),
            fillers: FILLERS
                .iter()
                .map(|&phrase| FillerCount {
                    phrase,
                    me: 0,
                    others: 0,
                })
                .collect(),
            ..StatsReport::default()
        };
        for m in meetings {
            let ms = m.duration_ms.unwrap_or(0);
            report.recorded_ms += ms;
            if ms > report.longest_meeting.as_ref().map_or(0, |r| r.ms) {
                report.longest_meeting = Some(Record {
                    meeting_id: m.id,
                    title: m.title.clone(),
                    speaker: None,
                    ms,
                });
            }
            if let Ok(at) = DateTime::parse_from_rfc3339(&m.started_at) {
                let at = at.with_timezone(&Local);
                let day = at.weekday().num_days_from_monday() as usize;
                report.week_hours[day][at.hour() as usize] += 1;
            }
        }
        let mut totals: Vec<PersonTotal> = Vec::new();
        for row in people {
            let Some(person_id) = row.person_id else {
                continue; // Me
            };
            let index = match totals.iter().position(|p| p.person_id == person_id) {
                Some(index) => index,
                None => {
                    totals.push(PersonTotal {
                        person_id,
                        name: row.person_name.clone(),
                        meetings: 0,
                        together_ms: 0,
                        talk_ms: 0,
                    });
                    totals.len() - 1
                }
            };
            let total = &mut totals[index];
            total.meetings += 1;
            total.together_ms += row.duration_ms;
            total.talk_ms += row.talk_ms;
        }
        totals.sort_by(|a, b| {
            b.together_ms
                .cmp(&a.together_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        report.people = totals;
        StatsBuilder {
            report,
            my_counts: HashMap::new(),
            their_counts: HashMap::new(),
            turn: None,
        }
    }

    /// Count one transcript line.
    pub fn add(&mut self, seg: &StatsSegmentRow) {
        let mine = seg.track == "mic";
        let words: Vec<String> = words(&seg.text).collect();
        let voice = if mine {
            &mut self.report.me
        } else {
            &mut self.report.others
        };
        voice.words += words.len() as u64;
        voice.talk_ms += (seg.end_ms - seg.start_ms).max(0);
        voice.questions += seg.text.matches('?').count() as u64;

        for filler in &mut self.report.fillers {
            let n = match filler.phrase.split_once(' ') {
                None => words.iter().filter(|w| *w == filler.phrase).count(),
                Some((a, b)) => words.windows(2).filter(|p| p[0] == a && p[1] == b).count(),
            } as u64;
            if mine {
                filler.me += n;
            } else {
                filler.others += n;
            }
        }
        let counts = if mine {
            &mut self.my_counts
        } else {
            &mut self.their_counts
        };
        for word in words.into_iter().filter(|w| cloud_worthy(w)) {
            *counts.entry(word).or_default() += 1;
        }

        // Turns: consecutive lines of one speaker, never across a pause
        // longer than MONOLOGUE_GAP_MS. Me is speaker None.
        let speaker = if mine {
            None
        } else {
            Some(seg.speaker_id.unwrap_or(-1))
        };
        let continues = self.turn.as_ref().is_some_and(|t| {
            t.meeting_id == seg.meeting_id
                && t.speaker == speaker
                && seg.start_ms - t.end_ms <= MONOLOGUE_GAP_MS
        });
        if continues {
            let turn = self.turn.as_mut().expect("turn");
            turn.end_ms = turn.end_ms.max(seg.end_ms);
        } else {
            self.close_turn();
            let name = if mine {
                "You".to_owned()
            } else {
                seg.speaker_name.clone().unwrap_or_else(|| "Someone".into())
            };
            self.turn = Some(Turn {
                meeting_id: seg.meeting_id,
                speaker,
                title: seg.meeting_title.clone(),
                name,
                start_ms: seg.start_ms,
                end_ms: seg.end_ms,
            });
        }
    }

    fn close_turn(&mut self) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        let ms = turn.end_ms - turn.start_ms;
        if ms > self.report.longest_monologue.as_ref().map_or(0, |r| r.ms) {
            self.report.longest_monologue = Some(Record {
                meeting_id: turn.meeting_id,
                title: turn.title,
                speaker: Some(turn.name),
                ms,
            });
        }
    }

    pub fn finish(mut self) -> StatsReport {
        self.close_turn();
        self.report.my_words = top_words(self.my_counts, CLOUD_WORDS);
        self.report.their_words = top_words(self.their_counts, CLOUD_WORDS);
        self.report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(
        meeting: i64,
        track: &str,
        speaker: Option<i64>,
        start: i64,
        end: i64,
        text: &str,
    ) -> StatsSegmentRow {
        StatsSegmentRow {
            meeting_id: meeting,
            meeting_title: format!("M{meeting}"),
            track: track.into(),
            speaker_id: speaker,
            speaker_name: speaker.map(|s| format!("Speaker {s}")),
            start_ms: start,
            end_ms: end,
            text: text.into(),
        }
    }

    #[test]
    fn words_split_and_normalise() {
        let got: Vec<String> = words("I\u{2019}m like, SO ready -- 'quoted' 42?").collect();
        assert_eq!(got, ["i'm", "like", "so", "ready", "quoted", "42"]);
        assert!(cloud_worthy("roadmap"));
        assert!(!cloud_worthy("the") && !cloud_worthy("2026") && !cloud_worthy("ok"));
    }

    #[test]
    fn fillers_voices_clouds_and_turns() {
        let mut b = StatsBuilder::new(&[], &[]);
        b.add(&seg(
            1,
            "mic",
            Some(10),
            0,
            31_000,
            "Like, you know, the roadmap? Like I mean it.",
        ));
        b.add(&seg(
            1,
            "mic",
            Some(10),
            31_000,
            60_000,
            "Roadmap roadmap budget.",
        ));
        b.add(&seg(
            1,
            "loopback",
            Some(11),
            60_500,
            70_000,
            "Budget, you know?",
        ));
        // A long pause ends the turn even for the same speaker.
        b.add(&seg(1, "loopback", Some(11), 90_000, 95_000, "So."));
        let r = b.finish();
        let filler = |p: &str| r.fillers.iter().find(|f| f.phrase == p).copied().unwrap();
        assert_eq!((filler("like").me, filler("like").others), (2, 0));
        assert_eq!((filler("you know").me, filler("you know").others), (1, 1));
        assert_eq!(filler("i mean").me, 1);
        assert_eq!(filler("so").others, 1);
        assert_eq!(r.me.words, 12);
        assert_eq!(r.me.talk_ms, 60_000);
        assert_eq!(r.me.questions, 1);
        assert_eq!(r.others.questions, 1);
        assert_eq!(r.me.wpm().map(|w| w.round()), Some(12.0));
        assert_eq!(r.others.wpm(), None, "under a minute of speech");
        assert_eq!(
            r.my_words,
            [
                WordCount {
                    word: "roadmap".into(),
                    count: 3
                },
                WordCount {
                    word: "budget".into(),
                    count: 1
                },
            ]
        );
        assert_eq!(r.their_words[0].word, "budget");
        let mono = r.longest_monologue.unwrap();
        assert_eq!((mono.speaker.as_deref(), mono.ms), (Some("You"), 60_000));
    }

    #[test]
    fn people_and_meetings_aggregate() {
        let stat = |person: Option<i64>, name: &str, meeting: i64, dur: i64, talk: i64| {
            PersonMeetingStat {
                person_id: person,
                person_name: name.into(),
                meeting_id: meeting,
                meeting_title: String::new(),
                started_at: String::new(),
                duration_ms: dur,
                talk_ms: talk,
                words: 0,
                segments: 0,
            }
        };
        let people = [
            stat(None, "Me", 1, 100, 50),
            stat(Some(2), "Bob", 1, 100, 10),
            stat(Some(3), "Cara", 2, 500, 20),
            stat(Some(2), "Bob", 2, 500, 30),
        ];
        let meeting = |id: i64, ms: Option<i64>| Meeting {
            id,
            started_at: "2026-10-05T09:30:00+00:00".into(),
            title: format!("M{id}"),
            duration_ms: ms,
            ..Meeting::default()
        };
        let r = StatsBuilder::new(
            &[
                meeting(1, Some(100)),
                meeting(2, Some(500)),
                meeting(3, None),
            ],
            &people,
        )
        .finish();
        assert_eq!(r.meetings, 3);
        assert_eq!(r.recorded_ms, 600);
        assert_eq!(r.longest_meeting.unwrap().meeting_id, 2);
        let names: Vec<_> = r
            .people
            .iter()
            .map(|p| (p.name.as_str(), p.meetings, p.together_ms))
            .collect();
        assert_eq!(names, [("Bob", 2, 600), ("Cara", 1, 500)]);
        assert_eq!(r.week_hours.iter().flatten().sum::<u32>(), 3);
    }
}
