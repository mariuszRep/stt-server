//! Who spoke when: `POST /v1/audio/diarize`.
//!
//! The pipeline is the server's own pieces in a row: speech segments (the caller's, or the VAD's
//! default detection), a CAM++ embedding per segment ([`super::embed`]), average-linkage
//! clustering of the embeddings ([`super::cluster::cluster_speakers`], the same algorithm as
//! stt-sdk's `clusterSpeakers`) and a label per cluster: a known speaker's name when the
//! cluster's voice matches a supplied profile, otherwise "A", "B", ... in order of first
//! appearance. The answer is shaped like OpenAI's `diarized_json` segments but carries no text.

use super::cluster::{cluster_speakers, cosine_similarity};
use super::{embed, normalise, samples_in, Segment, DIM, SAMPLE_RATE};

/// The `model` of the response and the `engine` advertised by `/v1/local/system`.
pub const ENGINE: &str = "campplus-voxceleb-v1-average-linkage";
/// Most speakers one recording is split into (the labels "A" to "Z"), and the most known speakers.
pub const MAX_SPEAKERS: usize = 26;
/// Two voices are the same speaker when their cosine similarity is at least this.
pub const DEFAULT_THRESHOLD: f64 = 0.4;
pub const THRESHOLDS: std::ops::RangeInclusive<f64> = 0.05..=0.95;
/// Most reference clips, and their length in seconds: OpenAI's limits for `known_speaker_references[]`.
pub const MAX_REFERENCES: usize = 4;
pub const REFERENCE_SECONDS: std::ops::RangeInclusive<f64> = 2.0..=10.0;
const MAX_NAME_CHARS: usize = 64;

/// The `threshold` field.
pub fn parse_threshold(raw: &str) -> Result<f64, String> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| THRESHOLDS.contains(value))
        .ok_or_else(|| {
            format!(
                "threshold must be a number from {} to {}",
                THRESHOLDS.start(),
                THRESHOLDS.end()
            )
        })
}

/// The `max_speakers` field.
pub fn parse_max_speakers(raw: &str) -> Result<usize, String> {
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=MAX_SPEAKERS).contains(value))
        .ok_or_else(|| format!("max_speakers must be a whole number from 1 to {MAX_SPEAKERS}"))
}

/// A voice the caller already knows: the name its segments get and its unit-vector embedding.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownSpeaker {
    pub name: String,
    pub embedding: Vec<f32>,
}

/// Checks that the three `known_speaker_*[]` lists fit together: embeddings or reference clips
/// (not both), one name for each, at most [`MAX_REFERENCES`] clips.
pub fn check_known_lists(
    names: &[String],
    embeddings: &[String],
    references: &[String],
) -> Result<(), String> {
    if !embeddings.is_empty() && !references.is_empty() {
        return Err(
            "send known_speaker_embeddings[] or known_speaker_references[], not both".to_owned(),
        );
    }
    if references.len() > MAX_REFERENCES {
        return Err(format!(
            "known_speaker_references[] may hold at most {MAX_REFERENCES} clips"
        ));
    }
    let (list, supplied) = if references.is_empty() {
        ("known_speaker_embeddings[]", embeddings.len())
    } else {
        ("known_speaker_references[]", references.len())
    };
    if names.len() != supplied {
        return Err(format!(
            "known_speaker_names[] has {} entries but {list} has {supplied}",
            names.len()
        ));
    }
    Ok(())
}

/// Checks `known_speaker_names[]`: at most [`MAX_SPEAKERS`] distinct non-empty names.
pub fn parse_names(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.len() > MAX_SPEAKERS {
        return Err(format!(
            "known_speaker_names[] may hold at most {MAX_SPEAKERS} names"
        ));
    }
    let mut names: Vec<String> = Vec::new();
    for name in raw.iter().map(|name| name.trim()) {
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Err(format!(
                "known_speaker_names[] entries must be 1 to {MAX_NAME_CHARS} characters"
            ));
        }
        if names.iter().any(|seen| seen == name) {
            return Err(format!("known_speaker_names[] repeats {name:?}"));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn base64_bytes(raw: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .map_err(|e| format!("not valid base64: {e}"))
}

/// One `known_speaker_embeddings[]` entry: base64 of [`DIM`] little-endian `f32` values (what
/// `POST /v1/audio/embeddings` returns with `encoding_format=base64`), normalised to a unit vector.
pub fn parse_embedding(raw: &str) -> Result<Vec<f32>, String> {
    let bytes = base64_bytes(raw)?;
    if bytes.len() != DIM * 4 {
        return Err(format!(
            "an embedding is {DIM} little-endian f32 values ({} bytes), not {} bytes",
            DIM * 4,
            bytes.len()
        ));
    }
    let mut embedding: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();
    if embedding.iter().any(|v| !v.is_finite()) || embedding.iter().all(|v| *v == 0.0) {
        return Err("an embedding needs finite values that are not all zero".to_owned());
    }
    normalise(&mut embedding);
    Ok(embedding)
}

/// One `known_speaker_references[]` entry: a WAV file as a `data:audio/wav;base64,...` URL
/// (OpenAI's form) or plain base64. Returns the file's bytes.
pub fn parse_reference(raw: &str) -> Result<Vec<u8>, String> {
    let raw = raw.trim();
    let payload = match raw.strip_prefix("data:") {
        Some(url) => {
            url.split_once(";base64,")
                .ok_or("a data URL must be base64 (data:audio/wav;base64,...)")?
                .1
        }
        None => raw,
    };
    base64_bytes(payload)
}

/// The profile of one reference clip (16 kHz mono samples), which must last [`REFERENCE_SECONDS`].
pub fn reference_embedding(samples: &[f32]) -> Result<Vec<f32>, String> {
    let seconds = samples.len() as f64 / f64::from(SAMPLE_RATE);
    if !REFERENCE_SECONDS.contains(&seconds) {
        return Err(format!(
            "a reference clip must last {} to {} seconds, not {seconds:.1}",
            REFERENCE_SECONDS.start(),
            REFERENCE_SECONDS.end()
        ));
    }
    Ok(embed(samples).expect("a reference clip is longer than the minimum"))
}

/// "A" to "Z", then "AA", "AB", ... for the `index`th unnamed speaker.
fn letters(index: usize) -> String {
    let mut label = String::new();
    let mut rest = index + 1;
    while rest > 0 {
        rest -= 1;
        label.insert(0, char::from(b'A' + (rest % 26) as u8));
        rest /= 26;
    }
    label
}

/// The mean of `vectors`, normalised.
fn centroid(vectors: &[&[f32]]) -> Vec<f32> {
    let mut mean = vec![0.0f32; DIM];
    for vector in vectors {
        mean.iter_mut().zip(*vector).for_each(|(m, v)| *m += v);
    }
    normalise(&mut mean);
    mean
}

/// Names the clusters: each known speaker takes the cluster it is most similar to (at least
/// `threshold`), the most similar pairs first and each name and cluster at most once; the rest
/// become "A", "B", ... in label order, skipping any name a known speaker has.
fn speaker_names(centroids: &[Vec<f32>], known: &[KnownSpeaker], threshold: f64) -> Vec<String> {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (cluster, centroid) in centroids.iter().enumerate() {
        for (index, speaker) in known.iter().enumerate() {
            let similarity = cosine_similarity(centroid, &speaker.embedding);
            if similarity >= threshold {
                pairs.push((similarity, cluster, index));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut names: Vec<Option<&str>> = vec![None; centroids.len()];
    let mut taken = vec![false; known.len()];
    for (_, cluster, index) in pairs {
        if names[cluster].is_none() && !taken[index] {
            names[cluster] = Some(&known[index].name);
            taken[index] = true;
        }
    }
    let mut unnamed = 0;
    names
        .into_iter()
        .map(|name| match name {
            Some(name) => name.to_owned(),
            None => loop {
                let label = letters(unnamed);
                unnamed += 1;
                if known.iter().all(|speaker| speaker.name != label) {
                    break label;
                }
            },
        })
        .collect()
}

/// What to diarize and how.
pub struct Request {
    /// Speech segments; `None` runs the VAD's default detection.
    pub segments: Option<Vec<Segment>>,
    pub threshold: f64,
    pub max_speakers: usize,
    pub known: Vec<KnownSpeaker>,
}

/// The `POST /v1/audio/diarize` response body for 16 kHz mono audio. A segment shorter than
/// [`super::MIN_SECONDS`] cannot be fingerprinted and is left out.
pub fn report(samples: &[f32], request: &Request) -> serde_json::Value {
    let seconds = |count: usize| count as f64 / f64::from(SAMPLE_RATE);
    let detected: Vec<Segment>;
    let segments = match &request.segments {
        Some(segments) => segments,
        None => {
            detected = crate::vad::detect(samples, &crate::vad::Params::default())
                .into_iter()
                .map(|span| Segment {
                    start: seconds(span.start),
                    end: seconds(span.end),
                })
                .collect();
            &detected
        }
    };
    let embeddings: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|segment| {
            let start = samples_in(segment.start).min(samples.len());
            let end = samples_in(segment.end).min(samples.len());
            embed(&samples[start..end])
        })
        .collect();
    let labels = cluster_speakers(&embeddings, request.threshold, request.max_speakers);

    let clusters = labels.iter().flatten().max().map_or(0, |last| last + 1);
    let centroids: Vec<Vec<f32>> = (0..clusters)
        .map(|cluster| {
            let members: Vec<&[f32]> = labels
                .iter()
                .zip(&embeddings)
                .filter(|(label, _)| **label == Some(cluster))
                .filter_map(|(_, embedding)| embedding.as_deref())
                .collect();
            centroid(&members)
        })
        .collect();
    let names = speaker_names(&centroids, &request.known, request.threshold);

    let mut entries = Vec::new();
    for (segment, label) in segments.iter().zip(&labels) {
        let Some(label) = label else { continue };
        entries.push(serde_json::json!({
            "type": "transcript.text.segment",
            "id": format!("seg_{}", entries.len()),
            "start": crate::vad::rounded(segment.start, 3),
            "end": crate::vad::rounded(segment.end.min(seconds(samples.len())), 3),
            "speaker": names[*label],
        }));
    }
    serde_json::json!({
        "model": ENGINE,
        "duration": crate::vad::rounded(seconds(samples.len()), 3),
        "segments": entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(direction: &[f32]) -> Vec<f32> {
        let mut vector = vec![0.0f32; DIM];
        vector[..direction.len()].copy_from_slice(direction);
        normalise(&mut vector);
        vector
    }

    fn encode(vector: &[f32]) -> String {
        use base64::Engine as _;
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn labels_run_a_to_z_then_two_letters() {
        let labels: Vec<String> = [0, 1, 25, 26, 27, 51, 52, 701, 702]
            .map(letters)
            .into_iter()
            .collect();
        assert_eq!(labels, ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"]);
    }

    #[test]
    fn numbers_are_range_checked() {
        assert_eq!(parse_threshold(" 0.5 "), Ok(0.5));
        for bad in ["", "x", "0.04", "0.96", "NaN", "inf"] {
            assert!(parse_threshold(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_max_speakers("26"), Ok(26));
        for bad in ["", "0", "27", "-1", "2.5", "x"] {
            assert!(parse_max_speakers(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_known_lists_must_fit_together() {
        let (a, b) = (strings(&["Ann"]), strings(&["x"]));
        assert!(check_known_lists(&[], &[], &[]).is_ok());
        assert!(check_known_lists(&a, &b, &[]).is_ok());
        assert!(check_known_lists(&a, &[], &b).is_ok());
        assert!(check_known_lists(&a, &[], &[]).is_err());
        assert!(check_known_lists(&[], &b, &[]).is_err());
        assert!(check_known_lists(&a, &b, &b).is_err());
        assert!(check_known_lists(&strings(&["a", "b"]), &b, &[]).is_err());
        let five = strings(&["1", "2", "3", "4", "5"]);
        assert!(check_known_lists(&five, &[], &five).is_err());
    }

    #[test]
    fn names_are_trimmed_and_must_be_distinct() {
        assert_eq!(
            parse_names(&strings(&[" Ann ", "Bob"])).unwrap(),
            ["Ann", "Bob"]
        );
        assert!(parse_names(&strings(&["Ann", "Ann"])).is_err());
        assert!(parse_names(&strings(&[" "])).is_err());
        assert!(parse_names(&[String::from("x").repeat(65)]).is_err());
    }

    #[test]
    fn embeddings_must_be_one_finite_vector_of_the_right_size() {
        let parsed = parse_embedding(&encode(&vec![2.0; DIM])).unwrap();
        assert!((super::super::dot(&parsed, &parsed) - 1.0).abs() < 1e-4);
        assert!(parse_embedding("!!").is_err());
        assert!(parse_embedding(&encode(&vec![1.0; DIM - 1])).is_err());
        assert!(parse_embedding(&encode(&vec![0.0; DIM])).is_err());
        assert!(parse_embedding(&encode(&vec![f32::NAN; DIM])).is_err());
    }

    #[test]
    fn references_take_a_data_url_or_plain_base64() {
        assert_eq!(
            parse_reference("data:audio/wav;base64,AQID").unwrap(),
            [1, 2, 3]
        );
        assert_eq!(parse_reference(" AQID ").unwrap(), [1, 2, 3]);
        assert!(parse_reference("data:audio/wav,AQID").is_err());
        assert!(parse_reference("data:audio/wav;base64,@@").is_err());
    }

    #[test]
    fn reference_clips_last_two_to_ten_seconds() {
        let seconds = |s: f64| vec![0.01f32; samples_in(s)];
        assert!(reference_embedding(&seconds(1.9)).is_err());
        assert!(reference_embedding(&seconds(10.1)).is_err());
        assert_eq!(reference_embedding(&seconds(2.0)).unwrap().len(), DIM);
    }

    #[test]
    fn known_speakers_take_the_closest_cluster_once_and_others_get_letters() {
        let centroids = [unit(&[1.0, 0.1]), unit(&[0.0, 1.0]), unit(&[-1.0, 0.0])];
        let speaker = |name: &str, embedding| KnownSpeaker {
            name: name.to_owned(),
            embedding,
        };
        // Both Ann and Cy resemble cluster 0; Ann is closer, so Cy gets nothing. Bob matches
        // cluster 1. Cluster 2 matches nobody, and the letter "A" belongs to a known name.
        let known = [
            speaker("Cy", unit(&[1.0, 0.5])),
            speaker("Ann", unit(&[1.0, 0.1])),
            speaker("Bob", unit(&[0.0, 1.0])),
            speaker("A", unit(&[0.0, 0.0, 1.0])),
        ];
        assert_eq!(speaker_names(&centroids, &known, 0.4), ["Ann", "Bob", "B"]);
        assert_eq!(speaker_names(&centroids, &[], 0.4), ["A", "B", "C"]);
    }
}
