use crate::events::{NormalizedEvent, RecordedEvent};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

pub struct EventRecorder {
    writer: BufWriter<File>,
}

impl EventRecorder {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    pub fn record(&mut self, event: &NormalizedEvent) -> Result<()> {
        let recorded = RecordedEvent::from_runtime(event);
        serde_json::to_writer(&mut self.writer, &recorded)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

pub fn read_recorded_events(path: impl AsRef<Path>) -> Result<Vec<NormalizedEvent>> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut first_ts = None;
    let base_instant = Instant::now();

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let recorded = match parse_recorded(path, idx + 1, &line) {
            Ok(recorded) => recorded,
            Err(err) if can_skip_truncated_tail(&err, &events) => break,
            Err(err) => return Err(err),
        };
        let first = *first_ts.get_or_insert(recorded.ts_millis());
        events.push(recorded.into_runtime(base_instant, first));
    }

    Ok(events)
}

fn parse_recorded(path: &Path, line_no: usize, line: &str) -> Result<RecordedEvent> {
    serde_json::from_str(line)
        .with_context(|| format!("failed to parse {}:{}", path.display(), line_no))
}

fn can_skip_truncated_tail(err: &anyhow::Error, events: &[NormalizedEvent]) -> bool {
    !events.is_empty()
        && err
            .chain()
            .any(|cause| cause.to_string().contains("EOF while parsing"))
}
