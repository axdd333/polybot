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

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let recorded: RecordedEvent = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse recorded event in {}", path.display()))?;
        let first = *first_ts.get_or_insert(recorded.ts_millis());
        events.push(recorded.into_runtime(base_instant, first));
    }

    Ok(events)
}
