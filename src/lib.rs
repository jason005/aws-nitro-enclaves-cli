// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
#![deny(missing_docs)]
#![deny(warnings)]

//! This crate provides the functionality for the Nitro CLI process.

/// The common module (shared between the CLI and enclave process).
pub mod common;
/// The enclave process module.
pub mod enclave_proc;
/// The module covering the communication between a CLI instance and enclave processes.
pub mod enclave_proc_comm;
/// The CLI-specific utilities module.
pub mod utils;

use log::debug;
use serde::Serialize;
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use common::commands_parser::{BuildEnclavesArgs, EmptyArgs};
use common::json_output::EnclaveTerminateInfo;
use common::{enclave_proc_command_send_single, get_sockets_dir_path};
use common::{EnclaveProcessCommandType, NitroCliErrorEnum, NitroCliFailure, NitroCliResult};
use enclave_proc_comm::enclave_process_handle_all_replies;
use log::info;

use utils::Console;

/// Hypervisor CID as defined by <http://man7.org/linux/man-pages/man7/vsock.7.html>.
pub const VMADDR_CID_HYPERVISOR: u32 = 0;

/// An offset applied to an enclave's CID in order to determine its console port.
pub const CID_TO_CONSOLE_PORT_OFFSET: u32 = 10000;

/// Default blobs path to be used if the corresponding environment variable is not set.
const DEFAULT_BLOBS_PATH: &str = "/usr/share/nitro_enclaves/blobs/";

/// Information obtained from a newly-build enclave image file.
#[derive(Serialize)]
pub struct EnclaveBuildInfo {
    #[serde(rename(serialize = "Measurements"))]
    measurements: BTreeMap<String, String>,
}

impl EnclaveBuildInfo {
    /// Construct a new `EnclaveBuildInfo` instance from the given measurements.
    pub fn new(measurements: BTreeMap<String, String>) -> Self {
        EnclaveBuildInfo { measurements }
    }
}

/// Build an enclave image file with the provided arguments.
pub fn build_enclaves(args: BuildEnclavesArgs) -> NitroCliResult<()> {
    debug!("build_enclaves");
    eprintln!("Start building the Enclave Image...");
    build_from_docker(
        &args.docker_uri,
        &args.docker_dir,
        &args.output,
        &args.signing_certificate,
        &args.private_key,
        None,
        None,
    )
    .map_err(|e| e.add_subaction("Failed to build EIF from docker".to_string()))?;
    Ok(())
}

/// Build an enclave image file from a Docker image.
pub fn build_from_docker(
    docker_uri: &str,
    docker_dir: &Option<String>,
    output_path: &str,
    signing_certificate: &Option<String>,
    private_key: &Option<String>,
    cert: Option<Vec<u8>>,
    priv_key: Option<Vec<u8>>,
) -> NitroCliResult<(File, BTreeMap<String, String>)> {
    let blobs_path =
        blobs_path().map_err(|e| e.add_subaction("Failed to retrieve blobs path".to_string()))?;
    let cmdline_file_path = format!("{}/cmdline", blobs_path);
    let mut cmdline_file = File::open(cmdline_file_path.clone()).map_err(|e| {
        new_nitro_cli_failure!(
            &format!("Could not open kernel command line file: {:?}", e),
            NitroCliErrorEnum::FileOperationFailure
        )
        .add_info(vec![&cmdline_file_path, "Open"])
    })?;

    let mut cmdline = String::new();
    cmdline_file.read_to_string(&mut cmdline).map_err(|e| {
        new_nitro_cli_failure!(
            &format!("Failed to read kernel command line: {:?}", e),
            NitroCliErrorEnum::FileOperationFailure
        )
        .add_info(vec![&cmdline_file_path, "Read"])
    })?;

    let mut file_output = OpenOptions::new()
        .read(true)
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_path)
        .map_err(|e| {
            new_nitro_cli_failure!(
                &format!("Could not create output file: {:?}", e),
                NitroCliErrorEnum::FileOperationFailure
            )
            .add_info(vec![output_path, "Open"])
        })?;

    let mut docker2eif = match (cert, priv_key) {
        (Some(cert), Some(key)) => enclave_build::Docker2Eif::new2(
            docker_uri.to_string(),
            format!("{}/init", blobs_path),
            format!("{}/nsm.ko", blobs_path),
            format!("{}/bzImage", blobs_path),
            cmdline.trim().to_string(),
            format!("{}/linuxkit", blobs_path),
            &mut file_output,
            artifacts_path()?,
            Some(cert),
            Some(key),
        )
        .map_err(|err| {
            new_nitro_cli_failure!(
                &format!("Failed to create EIF image: {:?}", err),
                NitroCliErrorEnum::EifBuildingError
            )
        })?,
        (_, _) => enclave_build::Docker2Eif::new(
            docker_uri.to_string(),
            format!("{}/init", blobs_path),
            format!("{}/nsm.ko", blobs_path),
            format!("{}/bzImage", blobs_path),
            cmdline.trim().to_string(),
            format!("{}/linuxkit", blobs_path),
            &mut file_output,
            artifacts_path()?,
            signing_certificate,
            private_key,
        )
        .map_err(|err| {
            new_nitro_cli_failure!(
                &format!("Failed to create EIF image: {:?}", err),
                NitroCliErrorEnum::EifBuildingError
            )
        })?,
    };

    if let Some(docker_dir) = docker_dir {
        docker2eif
            .build_docker_image(docker_dir.clone())
            .map_err(|err| {
                new_nitro_cli_failure!(
                    &format!("Failed to build docker image: {:?}", err),
                    NitroCliErrorEnum::DockerImageBuildError
                )
            })?;
    } else {
        docker2eif.pull_docker_image().map_err(|err| {
            new_nitro_cli_failure!(
                &format!("Failed to pull docker image: {:?}", err),
                NitroCliErrorEnum::DockerImagePullError
            )
        })?;
    }
    let measurements = docker2eif.create().map_err(|err| {
        new_nitro_cli_failure!(
            &format!("Failed to create EIF image: {:?}", err),
            NitroCliErrorEnum::EifBuildingError
        )
    })?;
    eprintln!("Enclave Image successfully created.");

    let info = EnclaveBuildInfo::new(measurements.clone());
    println!(
        "{}",
        serde_json::to_string_pretty(&info).map_err(|err| new_nitro_cli_failure!(
            &format!("Failed to display EnclaveBuild data: {:?}", err),
            NitroCliErrorEnum::SerdeError
        ))?
    );

    Ok((file_output, measurements))
}

/// Returns the value of the `NITRO_CLI_BLOBS` environment variable.
///
/// This variable specifies where all the blobs necessary for building
/// an enclave image are stored. As of now the blobs are:
/// - *bzImage*: A kernel image.
/// - *init*: The initial init process that is bootstraping the environment.
/// - *linuxkit*: A slightly modified version of linuxkit.
/// - *cmdline*: A file containing the kernel commandline.
fn blobs_path() -> NitroCliResult<String> {
    // TODO Improve error message with a suggestion to the user
    // consider using the default path used by rpm install
    let blobs_res = std::env::var("NITRO_CLI_BLOBS");

    Ok(blobs_res.unwrap_or_else(|_| DEFAULT_BLOBS_PATH.to_string()))
}

/// Returns the value of the `NITRO_CLI_ARTIFACTS` environment variable.
///
/// This variable configures the path where the build artifacts should be saved.
fn artifacts_path() -> NitroCliResult<String> {
    if let Ok(artifacts) = std::env::var("NITRO_CLI_ARTIFACTS") {
        std::fs::create_dir_all(artifacts.clone()).map_err(|e| {
            new_nitro_cli_failure!(
                &format!("Could not create artifacts path {}: {:?}", artifacts, e),
                NitroCliErrorEnum::FileOperationFailure
            )
            .add_info(vec![&artifacts, "Create"])
        })?;
        Ok(artifacts)
    } else if let Ok(home) = std::env::var("HOME") {
        let artifacts = format!("{}/.nitro_cli/", home);
        std::fs::create_dir_all(artifacts.clone()).map_err(|e| {
            new_nitro_cli_failure!(
                &format!("Could not create artifacts path {}: {:?}", artifacts, e),
                NitroCliErrorEnum::FileOperationFailure
            )
            .add_info(vec![&artifacts, "Create"])
        })?;
        Ok(artifacts)
    } else {
        Err(new_nitro_cli_failure!(
            "Could not find a folder for the CLI artifacts, set either HOME or NITRO_CLI_ARTIFACTS",
            NitroCliErrorEnum::ArtifactsPathNotSet
        ))
    }
}

/// Wrapper over the console connection function.
pub fn console_enclaves(enclave_cid: u64) -> NitroCliResult<()> {
    debug!("console_enclaves");
    println!("Connecting to the console for enclave {}...", enclave_cid);
    enclave_console(enclave_cid)?;
    Ok(())
}

/// Connects to the enclave console and prints it continously.
pub fn enclave_console(enclave_cid: u64) -> NitroCliResult<()> {
    let console = Console::new(
        VMADDR_CID_HYPERVISOR,
        u32::try_from(enclave_cid).map_err(|err| {
            new_nitro_cli_failure!(
                &format!("Failed to parse enclave CID: {:?}", err),
                NitroCliErrorEnum::IntegerParsingError
            )
        })? + CID_TO_CONSOLE_PORT_OFFSET,
    )
    .map_err(|e| e.add_subaction("Connect to enclave console".to_string()))?;
    println!("Successfully connected to the console.");
    console
        .read_to(io::stdout().by_ref())
        .map_err(|e| e.add_subaction("Connect to enclave console".to_string()))?;

    Ok(())
}

/// Terminates all enclave instances belonging to the current user (or all
/// instances, if the current user has `root` permissions).
pub fn terminate_all_enclaves() -> NitroCliResult<()> {
    let sockets_dir = get_sockets_dir_path();
    let mut replies: Vec<UnixStream> = vec![];
    let sockets = std::fs::read_dir(sockets_dir.as_path()).map_err(|e| {
        new_nitro_cli_failure!(
            &format!("Error while accessing sockets directory: {:?}", e),
            NitroCliErrorEnum::FileOperationFailure
        )
        .add_info(vec![
            sockets_dir
                .as_path()
                .to_str()
                .unwrap_or("Invalid unicode directory name"),
            "Read",
        ])
    })?;

    let mut err_socket_files: usize = 0;
    let mut failed_connections: Vec<PathBuf> = Vec::new();
    for socket in sockets {
        let entry = match socket {
            Ok(value) => value,
            Err(_) => {
                err_socket_files += 1;
                continue;
            }
        };

        // Send a `terminate-enclave` command through each socket,
        // irrespective of the enclave process owner. The security policy
        // inside the enclave process is responsible with checking the
        // command's permissions.
        let mut stream = match UnixStream::connect(entry.path()) {
            Ok(value) => value,
            Err(_) => {
                failed_connections.push(entry.path());
                continue;
            }
        };

        if enclave_proc_command_send_single::<EmptyArgs>(
            EnclaveProcessCommandType::Terminate,
            None,
            &mut stream,
        )
        .is_err()
        {
            failed_connections.push(entry.path());
        } else {
            replies.push(stream);
        }
    }

    // Remove stale socket files.
    for stale_socket in &failed_connections {
        info!("Deleting stale socket: {:?}", stale_socket);
        let _ = std::fs::remove_file(stale_socket);
    }

    enclave_process_handle_all_replies::<EnclaveTerminateInfo>(
        &mut replies,
        failed_connections.len() + err_socket_files,
        false,
        vec![0, libc::EACCES],
    )
    .map_err(|e| e.add_subaction("Failed to handle all enclave processes replies".to_string()))
}

/// Macro defining the arguments configuration for a *Nitro CLI* application.
#[macro_export]
macro_rules! create_app {
    () => {
        App::new("Nitro CLI")
            .about("CLI for enclave lifetime management")
            .setting(AppSettings::ArgRequiredElseHelp)
            .version(env!("CARGO_PKG_VERSION"))
            .subcommand(
                SubCommand::with_name("run-enclave")
                    .about("Starts a new enclave")
                    .arg(
                        Arg::with_name("cpu-ids")
                            .long("cpu-ids")
                            .help("List of cpu-ids that will be provided to the enclave")
                            .takes_value(true)
                            .multiple(true)
                            .min_values(1)
                            .required_unless("cpu-count")
                            .required_unless("config")
                            .conflicts_with("cpu-count")
                            .conflicts_with("config"),
                    )
                    .arg(
                        Arg::with_name("cpu-count")
                            .long("cpu-count")
                            .help("Number of cpus")
                            .takes_value(true)
                            .required_unless("cpu-ids")
                            .required_unless("config")
                            .conflicts_with("cpu-ids")
                            .conflicts_with("config"),
                    )
                    .arg(
                        Arg::with_name("memory")
                            .long("memory")
                            .help("Memory to allocate for the enclave in MB")
                            .required_unless("config")
                            .takes_value(true)
                            .conflicts_with("config"),
                    )
                    .arg(
                        Arg::with_name("eif-path")
                            .long("eif-path")
                            .help("Path pointing to a prebuilt Eif image")
                            .required_unless("config")
                            .takes_value(true)
                            .conflicts_with("config"),
                    )
                    .arg(
                        Arg::with_name("enclave-cid")
                            .long("enclave-cid")
                            .takes_value(true)
                            .help("CID to be used for the newly started enclave")
                            .conflicts_with("config"),
                    )
                    .arg(
                        Arg::with_name("debug-mode")
                            .long("debug-mode")
                            .takes_value(false)
                            .help(
                                "Starts enclave in debug-mode. This makes the console of the enclave \
                                available over vsock at CID: VMADDR_CID_HYPERVISOR (0), port: \
                                enclave_cid + 10000. \n The stream could be accessed with the console \
                                sub-command" ,
                            )
                            .conflicts_with("config"),
                    )
                    .arg(
                        Arg::with_name("config")
                            .long("config")
                            .takes_value(true)
                            .value_name("json-config")
                            .help("Config is used to read enclave settings from JSON file"),
                    ),
            )
            .subcommand(
                SubCommand::with_name("terminate-enclave")
                    .about("Terminates an enclave")
                    .arg(
                        Arg::with_name("enclave-id")
                            .long("enclave-id")
                            .takes_value(true)
                            .help("Enclave ID, used to uniquely identify an enclave")
                            .required(true)
                            .conflicts_with("all"),
                    )
                    .arg(
                        Arg::with_name("all")
                            .long("all")
                            .takes_value(false)
                            .help("Terminate all running enclave instances belonging to the current user")
                            .required(false)
                            .conflicts_with("enclave-id"),
                    ),
            )
            .subcommand(
                SubCommand::with_name("build-enclave")
                    .about("Builds an enclave image and saves it to a file")
                    .arg(
                        Arg::with_name("docker-uri")
                            .long("docker-uri")
                            .help(
                                "Uri pointing to an existing docker container or to be created  \
                                 locally when docker-dir is present",
                            )
                            .required(true)
                            .takes_value(true),
                    )
                    .arg(
                        Arg::with_name("docker-dir")
                            .long("docker-dir")
                            .help("Local path to a directory containing a Dockerfile")
                            .takes_value(true),
                    )
                    .arg(
                        Arg::with_name("output-file")
                            .long("output-file")
                            .help("Location where the Enclave Image should be saved")
                            .group("action")
                            .required(true)
                            .takes_value(true),
                    )
                    .arg(
                        Arg::with_name("signing-certificate")
                            .long("signing-certificate")
                            .help("Local path to developer's X509 signing certificate.")
                            .takes_value(true),
                    )
                    .arg(
                        Arg::with_name("private-key")
                            .long("private-key")
                            .help("Local path to developer's Eliptic Curve private key.")
                            .takes_value(true),
                    ),
            )
            .subcommand(
                SubCommand::with_name("describe-enclaves")
                    .about("Returns a list of the running enclaves"),
            )
            .subcommand(
                SubCommand::with_name("console")
                    .about("Connect to the console of an enclave")
                    .arg(
                        Arg::with_name("enclave-id")
                            .long("enclave-id")
                            .takes_value(true)
                            .help("Enclave ID, used to uniquely identify an enclave")
                            .required(true),
                    ),
            )
            .subcommand(
                SubCommand::with_name("explain")
                    .about("Display detailed information about an error returned by a misbehaving Nitro CLI command")
                    .arg(
                        Arg::with_name("error-code")
                            .long("error-code")
                            .takes_value(true)
                            .help("Error code, as returned by the misbehaving Nitro CLI command")
                            .required(true),
                    ),
            )
    };
}
