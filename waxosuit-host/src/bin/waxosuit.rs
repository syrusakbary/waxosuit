// Copyright 2015-2018 Capital One Services, LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use quicli::prelude::*;
use reqwest::StatusCode;

use std::fs::{read_dir, File};
use std::io::Read;
use std::path::PathBuf;
use structopt::StructOpt;
use wascap::jwt::validate_token;
use wascap::jwt::Claims;
use waxosuit_host::wasm::ModuleHost;
use waxosuit_host::capabilities::CAPMAN;


#[derive(Debug, StructOpt, Clone)]
#[structopt(
    raw(setting = "structopt::clap::AppSettings::ColoredHelp"),
    name = "waxosuit",
    about = "A WASCAP host runtime process for executing WebAssembly modules with secure capability bindings"
)]
struct Cli {
    /// Input file
    #[structopt(parse(from_os_str))]
    input: PathBuf,

    /// Directory from which to load capability providers
    #[structopt(parse(from_os_str), short = "c", long = "caps")]
    caps_dir: PathBuf,

    /// URL to POST a WebAssembly module's JWT for Open Policy Agent evaluation
    #[structopt(short = "o", long = "opa", env = "OPA_URL")]
    opa_url: Option<String>,

    /// The port on which to run the HTTP server
    #[structopt(short = "p", long = "port", default_value = "8080", env = "PORT")]
    port: u32,

    /// Used to indicate the sink URL when using a waxosuit module as a knative event emitter
    #[structopt(short = "s", long = "sink", env = "SINK")]
    sink: Option<String>,
}

fn main() -> Result<(), Box<dyn ::std::error::Error>> {
    let args = Cli::from_args();
    let inputfile = &args.input;
    env_logger::init();

    let buf = {
        let mut wfile = File::open(inputfile).unwrap();
        let mut buf = Vec::new();
        wfile.read_to_end(&mut buf).unwrap();
        buf
    };

    // Extract will return an error if it encounters an invalid hash in the claims
    let claims = wascap::wasm::extract_claims(&buf);
    match claims {
        Ok(Some(token)) => {
            let validate_res = validate_token(&token.jwt)?;
            if validate_res.cannot_use_yet {
                eprint!(
                    "Will not load WebAssembly module. Token is currently unusable. It will be usable {}",
                    validate_res.not_before_human
                );
                Ok(())
            } else if validate_res.expired {
                eprint!(
                    "Will not load WebAssembly module. Token expired {}",
                    validate_res.expires_human
                );
                Ok(())
            } else {
                check_token_with_opa(&args, token.jwt)?;
                start(&args, token.claims, buf).unwrap();
                Ok(())
            }
        }
        Ok(None) => {
            eprint!("No capability signature found");
            Ok(())
        }
        Err(e) => {
            eprint!("Error reading capabilities from file: {}", e);
            Err(Box::new(e))
        }
    }
}

fn start(args: &Cli, claims: Claims, buf: Vec<u8>) -> waxosuit_host::Result<()> {
    {
        let mut capman = CAPMAN.write().unwrap();
        capman.set_claims(claims.clone());
    }
    add_capabilities(&args.caps_dir, &claims.caps);

    let module_name: &str = args.input.file_stem().unwrap_or_default().to_str().unwrap();

    info!(
        "Starting Waxosuit for module {} with capability claims - {}",
        module_name,
        claims.caps.map_or("none".to_string(), |c| c.join(", "))
    );

    {
        let lock = CAPMAN.read().unwrap();
        lock.start_mux(move || {
            let host = ModuleHost::new(&buf).unwrap();
            Ok(host)
        })?;
    }

    std::thread::park();
    Ok(())
}

fn check_token_with_opa(args: &Cli, jwt: String) -> waxosuit_host::Result<()> {
    args.opa_url.as_ref().map_or(Ok(()), |url| {
        let postresult = post_json(url, &jwt)?;
        let oparesult: OpaReply = serde_json::from_str(&postresult)?;
        if oparesult.allow {
            info!("OPA validation PASSED");
            Ok(())
        } else {
            info!("OPA validation DENIED");
            Err(waxosuit_host::errors::new(
                waxosuit_host::errors::ErrorKind::WascapViolation(
                    "OPA denied this module".to_string(),
                ),
            ))
        }
    })
}

pub fn post_json(url: &str, token: &str) -> waxosuit_host::Result<String> {
    let client = reqwest::Client::new();
    let url = url.to_owned();
    let opapost = OpaInput {
        token: token.to_string(),
    };
    match client.post(&url).json(&opapost).send() {
        Ok(mut response) => {
            let raw = response.text()?;
            if response.status() == StatusCode::OK {
                Ok(raw)
            } else {
                Err(waxosuit_host::errors::new(
                    waxosuit_host::errors::ErrorKind::WascapViolation(
                        "Open Policy Agent did not return 200 OK".to_string(),
                    ),
                ))
            }
        }
        Err(e) => Err(waxosuit_host::errors::new(
            waxosuit_host::errors::ErrorKind::HttpClientFailure(e),
        )),
    }
}

fn add_capabilities(caps_dir: &PathBuf, caps: &Option<Vec<String>>) {
    if caps_dir.is_dir() {
        for entry in read_dir(caps_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if !path.is_dir() {
                match path.extension().map(|ex| ex.to_str().unwrap()) {
                    Some("dylib") | Some("so") => {
                        let result = {
                            unsafe {
                                let mut capman = CAPMAN.write().unwrap();
                                capman.load_plugin(path)
                            }
                        };
                        match result {
                            Ok(capid) => {
                                info!("Capability provider {} loaded", capid);
                            }
                            Err(e) => {
                                info!("Capability provider not loaded: {}", e);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    } else {
        panic!("Capability plugin location is not a directory.");
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct OpaInput {
    token: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct OpaReply {
    allow: bool,
}