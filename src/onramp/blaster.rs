// Copyright 2018-2020, Wayfair GmbH
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::dflt;
use crate::onramp::prelude::*;
use async_std::sync::{channel, Receiver};
use serde_yaml::Value;
use std::fs::File;
use std::io::{BufRead, Read};
use std::path::Path;
use std::thread;
use std::time::Duration;
use xz2::read::XzDecoder;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    /// source file to read data from, it will be iterated over repeatedly,
    /// can be xz compressed
    pub source: String,
    /// Interval in nanoseconds for coordinated emission testing
    pub interval: Option<u64>,
    /// Number of iterations to stop after
    pub iters: Option<u64>,
    #[serde(default = "dflt::d_false")]
    pub base64: bool,
}

impl ConfigImpl for Config {}

#[derive(Clone)]
pub struct Blaster {
    pub config: Config,
    data: Vec<u8>,
    acc: Acc,
}

impl onramp::Impl for Blaster {
    fn from_config(config: &Option<Value>) -> Result<Box<dyn Onramp>> {
        if let Some(config) = config {
            let config: Config = Config::new(config)?;
            let mut source_data_file = File::open(&config.source)?;
            let mut data = vec![];
            let ext = Path::new(&config.source)
                .extension()
                .map(std::ffi::OsStr::to_str);
            if ext == Some(Some("xz")) {
                XzDecoder::new(source_data_file).read_to_end(&mut data)?;
            } else {
                source_data_file.read_to_end(&mut data)?;
            };
            Ok(Box::new(Self {
                config,
                data,
                acc: Acc::default(),
            }))
        } else {
            Err("Missing config for blaster onramp".into())
        }
    }
}

#[derive(Clone, Default)]
struct Acc {
    elements: Vec<Vec<u8>>,
    consuming: Vec<Vec<u8>>,
    count: u64,
}

// We got to allow this because of the way that the onramp works
// with iterating over the data.
#[allow(clippy::needless_pass_by_value)]
fn onramp_loop(
    mut source: Blaster,
    rx: &Receiver<onramp::Msg>,
    mut preprocessors: Preprocessors,
    mut codec: Box<dyn Codec>,
    mut metrics_reporter: RampReporter,
) -> Result<()> {
    let mut pipelines: Vec<(TremorURL, pipeline::Addr)> = Vec::new();
    task::block_on(source.init())?;

    let origin_uri = tremor_pipeline::EventOriginUri {
        scheme: "tremor-blaster".to_string(),
        host: hostname(),
        port: None,
        path: vec![source.config.source.clone()],
    };

    let mut id = 0;
    loop {
        match task::block_on(handle_pipelines(&rx, &mut pipelines, &mut metrics_reporter))? {
            PipeHandlerResult::Retry => continue,
            PipeHandlerResult::Terminate => return Ok(()),
            PipeHandlerResult::Normal => (),
        }
        match task::block_on(source.read())? {
            SourceReply::Data(data) => {
                let mut ingest_ns = nanotime();
                send_event(
                    &pipelines,
                    &mut preprocessors,
                    &mut codec,
                    &mut metrics_reporter,
                    &mut ingest_ns,
                    &origin_uri,
                    id,
                    data,
                );
                id += 1;
            }
            SourceReply::StateChange(SourceState::Disconnected) => return Ok(()),
            SourceReply::StateChange(SourceState::Connected) => (),
        }
    }
}

#[async_trait::async_trait()]
impl Source for Blaster {
    async fn read(&mut self) -> Result<SourceReply> {
        // TODO better sleep perhaps
        if let Some(ival) = self.config.interval {
            thread::sleep(Duration::from_nanos(ival));
        }
        if Some(self.acc.count) == self.config.iters {
            return Ok(SourceReply::StateChange(SourceState::Disconnected));
        };
        self.acc.count += 1;
        if self.acc.consuming.is_empty() {
            self.acc.consuming = self.acc.elements.clone();
        }

        if let Some(data) = self.acc.consuming.pop() {
            Ok(SourceReply::Data(data))
        } else {
            Ok(SourceReply::StateChange(SourceState::Disconnected))
        }
    }
    async fn init(&mut self) -> Result<SourceState> {
        let elements: Result<Vec<Vec<u8>>> = self
            .data
            .lines()
            .map(|e| -> Result<Vec<u8>> {
                if self.config.base64 {
                    Ok(base64::decode(&e?.as_bytes())?)
                } else {
                    Ok(e?.as_bytes().to_vec())
                }
            })
            .collect();
        self.acc.elements = elements?;
        self.acc.consuming = self.acc.elements.clone();
        Ok(SourceState::Connected)
    }
}

impl Onramp for Blaster {
    fn start(
        &mut self,
        codec: &str,
        preprocessors: &[String],
        metrics_reporter: RampReporter,
    ) -> Result<onramp::Addr> {
        let (tx, rx) = channel(1);
        let codec = codec::lookup(&codec)?;
        let preprocessors = make_preprocessors(&preprocessors)?;
        let source = self.clone();
        thread::Builder::new()
            .name(format!("onramp-blaster-{}", "???"))
            .spawn(move || onramp_loop(source, &rx, preprocessors, codec, metrics_reporter))?;
        Ok(tx)
    }

    fn default_codec(&self) -> &str {
        "json"
    }
}
