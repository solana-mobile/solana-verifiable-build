use anyhow::anyhow;
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use solana_address::Address;
use solana_rpc_client::rpc_client::RpcClient;
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::{process::Command, sync::atomic::Ordering};

use solana_verify::{
    api::{get_remote_job, get_remote_status, send_job_with_uploader_to_remote},
    build, export_pda_tx, get_buffer_hash, get_file_hash, get_program_hash,
    solana_program::{
        get_all_pdas_available, get_program_pda, process_close, resolve_rpc_url,
        validate_config_and_keypair, OtterBuildParams,
    },
    verify_from_image, verify_from_repo, SIGNAL_RECEIVED,
};

#[cfg(test)]
mod test;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle SIGTERM and SIGINT gracefully by stopping the docker container
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    let mut container_id: Option<String> = None;
    let mut temp_dir: Option<String> = None;

    let handle = signals.handle();
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            SIGNAL_RECEIVED.store(true, Ordering::Relaxed);
        }
    });

    // Add a function to check if we should abort
    let check_signal = |container_id: &mut Option<String>, temp_dir: &mut Option<String>| {
        if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
            println!("\nReceived interrupt signal, cleaning up...");

            if let Some(container_id) = container_id.take() {
                if std::process::Command::new("docker")
                    .args(["kill", &container_id])
                    .output()
                    .is_err()
                {
                    println!("Failed to close docker container");
                } else {
                    println!("Stopped container {container_id}")
                }
            }

            if let Some(temp_dir) = temp_dir.take() {
                if std::process::Command::new("rm")
                    .args(["-rf", &temp_dir])
                    .output()
                    .is_err()
                {
                    println!("Failed to remove temporary directory");
                } else {
                    println!("Removed temporary directory {temp_dir}");
                }
            }

            std::process::exit(130);
        }
    };

    let matches = App::new("solana-verify")
        .author("Ellipsis Labs <maintainers@ellipsislabs.xyz>")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A CLI tool for building verifiable Solana programs")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg(Arg::with_name("url")
            .short("u")
            .long("url")
            .global(true)
            .takes_value(true)
            .help("Optionally include your RPC endpoint. Defaults to Solana CLI config file"))
        .arg(Arg::with_name("compute-unit-price")
            .long("compute-unit-price")
            .global(true)
            .takes_value(true)
            .default_value("100000")
            .help("Priority fee in micro-lamports per compute unit"))
        .arg(Arg::with_name("config")
            .short("c")
            .long("config")
            .global(true)
            .takes_value(true)
            .help("Specify a custom configuration file path"))
        .subcommand(SubCommand::with_name("build")
            .about("Deterministically build the program in a Docker container")
            .arg(Arg::with_name("mount-directory")
                .help("Path to mount to the docker image")
                .takes_value(true))
            .arg(Arg::with_name("workspace-path")
                .long("workspace-path")
                .takes_value(true)
                .help("Path to the workspace root (for monorepos). Defaults to mount path. Use when the program is in a separate workspace that references other crates in the repo."))
            .arg(Arg::with_name("library-name")
                .long("library-name")
                .takes_value(true)
                .help("Which binary file to build"))
            .arg(Arg::with_name("base-image")
                .short("b")
                .long("base-image")
                .takes_value(true)
                .help("Optionally specify a custom base docker image to use for building"))
            .arg(Arg::with_name("bpf")
                .long("bpf")
                .help("If the program requires cargo build-bpf (instead of cargo build-sbf), set this flag"))
            .arg(Arg::with_name("arch")
                .long("arch")
                .takes_value(true)
                .possible_values(&["v0", "v1", "v2", "v3"])
                .help("Build for the given target architecture [default: v0]"))
            .arg(Arg::with_name("cargo-build-sbf-args")
                .long("cargo-build-sbf-args")
                .takes_value(true)
                .require_equals(true)
                .value_name("ARGS")
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("cargo-args")
                .multiple(true)
                .last(true)
                .help("Arguments to pass to the underlying `cargo` command")))
        .subcommand(SubCommand::with_name("verify-from-image")
            .about("Verifies a cached build from a docker image")
            .arg(Arg::with_name("executable-path-in-image")
                .short("e")
                .long("executable-path-in-image")
                .takes_value(true)
                .required(true)
                .help("Path to the executable solana program within the source code repository in the docker image"))
            .arg(Arg::with_name("image")
                .short("i")
                .long("image")
                .takes_value(true)
                .required(true)
                .help("Image that contains the source code to be verified"))
            .arg(Arg::with_name("program-id")
                .short("p")
                .long("program-id")
                .takes_value(true)
                .required(true)
                .help("The Program ID of the program to verify"))
            .arg(Arg::with_name("current-dir")
                .long("current-dir")
                .help("Verify in current directory")))
        .subcommand(SubCommand::with_name("get-executable-hash")
            .about("Get the hash of a program binary from an executable file")
            .arg(Arg::with_name("filepath")
                .required(true)
                .help("Path to the executable solana program")))
        .subcommand(SubCommand::with_name("get-program-hash")
            .about("Get the hash of a program binary from the deployed on-chain program")
            .arg(Arg::with_name("program-id")
                .required(true)
                .help("The Program ID of the program to verify")))
        .subcommand(SubCommand::with_name("get-buffer-hash")
            .about("Get the hash of a program binary from the deployed buffer address")
            .arg(Arg::with_name("buffer-address")
                .required(true)
                .help("Address of the buffer account containing the deployed program data")))
        .subcommand(SubCommand::with_name("verify-from-repo")
            .about("Builds and verifies a program from a given repository URL and a program ID")
            .arg(Arg::with_name("remote")
                .long("remote")
                .help("Send the verify command to a remote machine")
                .default_value("false")
                .takes_value(false))
            .arg(Arg::with_name("mount-path")
                .long("mount-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the root directory or the source code repository from which to build the program"))
            .arg(Arg::with_name("workspace-path")
                .long("workspace-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the program workspace (for monorepos). Use when the program is in a separate workspace that references other crates. Defaults to mount-path."))
            .arg(Arg::with_name("repo-url")
                .required(true)
                .help("The HTTPS URL of the repo to clone"))
            .arg(Arg::with_name("commit-hash")
                .long("commit-hash")
                .takes_value(true)
                .help("Commit hash to checkout. Required to know the correct program snapshot. Will fallback to HEAD if not provided"))
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
                .help("The Program ID of the program to verify"))
            .arg(Arg::with_name("base-image")
                .short("b")
                .long("base-image")
                .takes_value(true)
                .help("Optionally specify a custom base docker image to use for building"))
            .arg(Arg::with_name("library-name")
                .long("library-name")
                .takes_value(true)
                .help("Specify the name of the library to build and verify"))
            .arg(Arg::with_name("bpf")
                .long("bpf")
                .help("If the program requires cargo build-bpf (instead of cargo build-sbf), set this flag"))
            .arg(Arg::with_name("arch")
                .long("arch")
                .takes_value(true)
                .possible_values(&["v0", "v1", "v2", "v3"])
                .help("Build for the given target architecture [default: v0]"))
            .arg(Arg::with_name("current-dir")
                .long("current-dir")
                .help("Verify in current directory"))
            .arg(Arg::with_name("skip-prompt")
                .short("y")
                .long("skip-prompt")
                .help("Skip the prompt to write verify data on chain without user confirmation"))
            .arg(Arg::with_name("keypair")
                .short("k")
                .long("keypair")
                .takes_value(true)
                .help("Optionally specify a keypair to use for uploading the program verification args"))
            .arg(Arg::with_name("cargo-build-sbf-args")
                .long("cargo-build-sbf-args")
                .takes_value(true)
                .require_equals(true)
                .value_name("ARGS")
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("cargo-args")
                .multiple(true)
                .last(true)
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("skip-build")
                .long("skip-build")
                .help("Skip building and verification, only upload the PDA")
                .takes_value(false)))
        .subcommand(SubCommand::with_name("export-pda-tx")
            .about("Export the transaction as base58 for use with Squads")
            .arg(Arg::with_name("uploader")
                .long("uploader")
                .takes_value(true)
                .required(true)
                .help("Specifies an address to use for uploading the program verification args (should be the program authority)"))
            .arg(Arg::with_name("encoding")
                .long("encoding")
                .takes_value(true)
                .default_value("base58")
                .possible_values(&["base58", "base64"])
                .help("The encoding to use for the transaction"))   
            .arg(Arg::with_name("mount-path")
                .long("mount-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the root directory or the source code repository from which to build the program"))
            .arg(Arg::with_name("workspace-path")
                .long("workspace-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the program workspace (for monorepos). Use when the program is in a separate workspace that references other crates. Defaults to mount-path."))
            .arg(Arg::with_name("repo-url")
                .required(true)
                .help("The HTTPS URL of the repo to clone"))
            .arg(Arg::with_name("commit-hash")
                .long("commit-hash")
                .takes_value(true)
                .help("Commit hash to checkout. Required to know the correct program snapshot. Will fallback to HEAD if not provided"))
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
                .help("The Program ID of the program to verify"))
            .arg(Arg::with_name("base-image")
                .short("b")
                .long("base-image")
                .takes_value(true)
                .help("Optionally specify a custom base docker image to use for building"))
            .arg(Arg::with_name("library-name")
                .long("library-name")
                .takes_value(true)
                .help("Specify the name of the library to build and verify"))
            .arg(Arg::with_name("bpf")
                .long("bpf")
                .help("If the program requires cargo build-bpf (instead of cargo build-sbf), set this flag"))
            .arg(Arg::with_name("arch")
                .long("arch")
                .takes_value(true)
                .possible_values(&["v0", "v1", "v2", "v3"])
                .help("Build for the given target architecture [default: v0]"))
            .arg(Arg::with_name("cargo-build-sbf-args")
                .long("cargo-build-sbf-args")
                .takes_value(true)
                .require_equals(true)
                .value_name("ARGS")
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("cargo-args")
                .multiple(true)
                .last(true)
                .help("Arguments to pass to the underlying `cargo build-sbf` command")))
        .subcommand(SubCommand::with_name("close")
            .about("Close the otter-verify PDA account associated with the given program ID")
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
                .help("The address of the program to close the PDA")))
            .arg(Arg::with_name("export")
                .long("export")
                .required(false)
                .help("Print the transaction as base58 for use with Squads"))
        .subcommand(SubCommand::with_name("list-program-pdas")
            .about("List all the PDA information associated with a program ID. Requires custom RPC endpoint")
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)))
        .subcommand(SubCommand::with_name("get-program-pda")
            .about("Get uploaded PDA information for a given program ID and signer")
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
            )
            .arg(Arg::with_name("signer")
                .short("s")
                .long("signer")
                .required(false)
                .takes_value(true)
                .help("Signer to get the PDA for")
            )
        )
        .subcommand(SubCommand::with_name("remote")
            .about("Send a command to a remote machine")
        .setting(AppSettings::SubcommandRequiredElseHelp)
            .subcommand(SubCommand::with_name("get-status")
                .about("Get the verification status of a program")
                .arg(Arg::with_name("program-id")
                    .long("program-id")
                    .required(true)
                    .takes_value(true)
                    .help("The program address to fetch verification status for")))

            .subcommand(SubCommand::with_name("get-job")
                .about("Get the status of a verification job")
                .arg(Arg::with_name("job-id")
                    .long("job-id")
                    .required(true)
                    .takes_value(true)))
            .subcommand(SubCommand::with_name("submit-job")
                .about("Submit a verification job with with on-chain information")
                .arg(Arg::with_name("program-id")
                    .long("program-id")
                    .required(true)
                    .takes_value(true))
                .arg(Arg::with_name("uploader")
                    .long("uploader")
                    .required(true)
                    .takes_value(true)
                    .help("This is the address that uploaded verified build information for the program-id")))
        )
        .get_matches();

    // Validate configuration early if custom config is provided
    let config_path = matches.value_of("config").map(|s| s.to_string());
    if config_path.is_some() {
        // Check if verify-from-repo subcommand has a keypair parameter
        let keypair_path = if let ("verify-from-repo", Some(sub_m)) = matches.subcommand() {
            sub_m.value_of("keypair").map(|s| s.to_string())
        } else {
            None
        };
        validate_config_and_keypair(config_path.as_deref(), keypair_path.as_deref())?;
    }

    let connection = resolve_rpc_url(
        matches.value_of("url").map(|s| s.to_string()),
        config_path.clone(),
    )?;
    let res = match matches.subcommand() {
        ("build", Some(sub_m)) => {
            let mount_directory = sub_m.value_of("mount-directory").map(|s| s.to_string());
            let workspace_path = sub_m.value_of("workspace-path").map(|s| s.to_string());
            let library_name = sub_m.value_of("library-name").map(|s| s.to_string());
            let base_image = sub_m.value_of("base-image").map(|s| s.to_string());
            let bpf_flag = sub_m.is_present("bpf");
            let arch = sub_m.value_of("arch").map(|s| s.to_string());
            let cargo_build_sbf_args = sub_m
                .value_of("cargo-build-sbf-args")
                .map(|s| s.to_string());
            let cargo_args = sub_m
                .values_of("cargo-args")
                .unwrap_or_default()
                .map(|s| s.to_string())
                .collect();
            build(
                mount_directory,
                workspace_path,
                library_name,
                base_image,
                bpf_flag,
                arch,
                cargo_build_sbf_args,
                cargo_args,
                &mut container_id,
            )
        }
        ("verify-from-image", Some(sub_m)) => {
            let executable_path = sub_m.value_of("executable-path-in-image").unwrap();
            let image = sub_m.value_of("image").unwrap();
            let program_id = sub_m.value_of("program-id").unwrap();
            let current_dir = sub_m.is_present("current-dir");
            verify_from_image(
                executable_path.to_string(),
                image.to_string(),
                matches.value_of("url").map(|s| s.to_string()),
                config_path.clone(),
                Address::try_from(program_id)?,
                current_dir,
                &mut temp_dir,
                &mut container_id,
            )
        }
        ("get-executable-hash", Some(sub_m)) => {
            let filepath = sub_m.value_of("filepath").map(|s| s.to_string()).unwrap();
            let program_hash = get_file_hash(&filepath)?;
            println!("{program_hash}");
            Ok(())
        }
        ("get-buffer-hash", Some(sub_m)) => {
            let buffer_address = sub_m.value_of("buffer-address").unwrap();
            let buffer_hash = get_buffer_hash(
                matches.value_of("url").map(|s| s.to_string()),
                Address::try_from(buffer_address)?,
            )?;
            println!("{buffer_hash}");
            Ok(())
        }
        ("get-program-hash", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            let program_hash = get_program_hash(&connection, Address::try_from(program_id)?)?;
            println!("{program_hash}");
            Ok(())
        }
        ("verify-from-repo", Some(sub_m)) => {
            let skip_build = sub_m.is_present("skip-build");
            let remote = sub_m.is_present("remote");
            let mount_path = sub_m.value_of("mount-path").map(|s| s.to_string()).unwrap();
            let workspace_path = sub_m
                .value_of("workspace-path")
                .map(|s| s.to_string())
                .unwrap_or_default();
            let repo_url = sub_m.value_of("repo-url").map(|s| s.to_string()).unwrap();
            let program_id = sub_m.value_of("program-id").unwrap();
            if remote {
                return Err(anyhow!(
                    "The --remote flag has been deprecated. Upload your verify PDA with programs upgrade authority, then queue the remote worker with `solana-verify remote submit-job --program-id {program_id} --uploader <UPLOADER>`. See https://solana.com/docs/programs/verified-builds for the full workflow."
                ));
            }
            let base_image = sub_m.value_of("base-image").map(|s| s.to_string());
            let library_name = sub_m.value_of("library-name").map(|s| s.to_string());
            let bpf_flag = sub_m.is_present("bpf");
            let arch = sub_m.value_of("arch").map(|s| s.to_string());
            let current_dir = sub_m.is_present("current-dir");
            let skip_prompt = sub_m.is_present("skip-prompt");
            let path_to_keypair = sub_m.value_of("keypair").map(|s| s.to_string());
            let compute_unit_price = matches
                .value_of("compute-unit-price")
                .unwrap()
                .parse::<u64>()
                .unwrap_or(100000);
            let cargo_build_sbf_args = sub_m
                .value_of("cargo-build-sbf-args")
                .map(|s| s.to_string());
            let cargo_args: Vec<String> = sub_m
                .values_of("cargo-args")
                .unwrap_or_default()
                .map(|s| s.to_string())
                .collect();

            let commit_hash = get_commit_hash(sub_m, &repo_url)?;

            println!("Skipping prompt: {skip_prompt}");
            verify_from_repo(
                mount_path,
                workspace_path,
                &connection,
                repo_url,
                Some(commit_hash),
                Address::try_from(program_id)?,
                base_image,
                library_name,
                bpf_flag,
                arch,
                cargo_build_sbf_args,
                cargo_args,
                current_dir,
                skip_prompt,
                path_to_keypair,
                compute_unit_price,
                skip_build,
                &mut container_id,
                &mut temp_dir,
                &check_signal,
                config_path.clone(),
            )
            .await
        }
        ("close", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            let compute_unit_price = matches
                .value_of("compute-unit-price")
                .unwrap()
                .parse::<u64>()
                .unwrap_or(100000);
            process_close(
                Address::try_from(program_id)?,
                &connection,
                compute_unit_price,
                config_path.clone(),
            )
            .await
        }
        ("export-pda-tx", Some(sub_m)) => {
            let uploader = sub_m.value_of("uploader").unwrap();
            let mount_path = sub_m.value_of("mount-path").map(|s| s.to_string()).unwrap();
            let workspace_path = sub_m
                .value_of("workspace-path")
                .map(|s| s.to_string())
                .unwrap_or_default();
            let repo_url = sub_m.value_of("repo-url").map(|s| s.to_string()).unwrap();
            let program_id = sub_m.value_of("program-id").unwrap();
            let base_image = sub_m.value_of("base-image").map(|s| s.to_string());
            let library_name = sub_m.value_of("library-name").map(|s| s.to_string());
            let bpf_flag = sub_m.is_present("bpf");
            let arch = sub_m.value_of("arch").map(|s| s.to_string());
            let encoding = sub_m.value_of("encoding").unwrap();

            let encoding: UiTransactionEncoding = match encoding {
                "base58" => UiTransactionEncoding::Base58,
                "base64" => UiTransactionEncoding::Base64,
                _ => {
                    return Err(anyhow!("Unsupported encoding: {}", encoding));
                }
            };

            let compute_unit_price = matches
                .value_of("compute-unit-price")
                .unwrap()
                .parse::<u64>()
                .unwrap_or(100000);

            let commit_hash = get_commit_hash(sub_m, &repo_url)?;
            let cargo_build_sbf_args = sub_m
                .value_of("cargo-build-sbf-args")
                .map(|s| s.to_string());
            let cargo_args: Vec<String> = sub_m
                .values_of("cargo-args")
                .unwrap_or_default()
                .map(|s| s.to_string())
                .collect();

            let connection = resolve_rpc_url(
                matches.value_of("url").map(|s| s.to_string()),
                config_path.clone(),
            )?;
            println!("Using connection url: {}", connection.url());

            export_pda_tx(
                &connection,
                Address::try_from(program_id)?,
                Address::try_from(uploader)?,
                repo_url,
                commit_hash,
                mount_path,
                workspace_path,
                library_name,
                base_image,
                bpf_flag,
                arch,
                &mut temp_dir,
                encoding,
                cargo_build_sbf_args,
                cargo_args,
                compute_unit_price,
            )
            .await
        }
        ("list-program-pdas", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            list_program_pdas(Address::try_from(program_id)?, &connection).await
        }
        ("get-program-pda", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            let signer = sub_m.value_of("signer").map(|s| s.to_string());
            print_program_pda(
                Address::try_from(program_id)?,
                signer,
                &connection,
                config_path.clone(),
            )
            .await
        }
        ("remote", Some(sub_m)) => match sub_m.subcommand() {
            ("get-status", Some(sub_m)) => {
                let program_id = sub_m.value_of("program-id").unwrap();
                get_remote_status(Address::try_from(program_id)?).await
            }
            ("get-job", Some(sub_m)) => {
                let job_id = sub_m.value_of("job-id").unwrap();
                get_remote_job(job_id).await
            }
            ("submit-job", Some(sub_m)) => {
                let program_id = sub_m.value_of("program-id").unwrap();
                let uploader = sub_m.value_of("uploader").unwrap();

                send_job_with_uploader_to_remote(
                    &connection,
                    &Address::try_from(program_id)?,
                    &Address::try_from(uploader)?,
                )
                .await
            }
            _ => unreachable!(),
        },
        // Handle other subcommands in a similar manner, for now let's panic
        _ => panic!(
            "Unknown subcommand: {:?}\nUse '--help' to see available commands",
            matches.subcommand().0
        ),
    };

    handle.close();
    res
}

fn get_commit_hash_from_remote(repo_url: &str) -> anyhow::Result<String> {
    // Fetch the symbolic reference of the default branch
    let output = Command::new("git")
        .arg("ls-remote")
        .arg("--symref")
        .arg(repo_url)
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to fetch repository information using git.\nError: {}",
                e
            )
        })?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch default branch information from repository '{}'.\nGit error: {}",
            repo_url,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Find out if the branch is called master or main
    let output_str = String::from_utf8(output.stdout)?;
    let default_branch = output_str
        .lines()
        .find_map(|line| {
            if line.starts_with("ref: refs/heads/") {
                Some(
                    line.trim_start_matches("ref: refs/heads/")
                        .split_whitespace()
                        .next()?
                        .to_string(),
                )
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unable to determine default branch from repository '{}'",
                repo_url
            )
        })?;

    println!("Default branch detected: {default_branch}");

    // Fetch the latest commit hash for the default branch
    let hash_output = Command::new("git")
        .arg("ls-remote")
        .arg(repo_url)
        .arg(&default_branch)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to fetch commit hash for default branch '{default_branch}' from repository '{repo_url}'.\nError: {e}"))?;

    if !hash_output.status.success() {
        let stderr = String::from_utf8_lossy(&hash_output.stderr);
        return Err(anyhow::anyhow!(
            "Failed to fetch commit hash for branch '{default_branch}' from repository '{repo_url}'.\nGit error: {stderr}"
        ));
    }

    // Parse and return the commit hash
    String::from_utf8(hash_output.stdout)?
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse commit hash from git ls-remote output"))
}

pub fn print_build_params(address: &Address, build_params: &OtterBuildParams) {
    println!("----------------------------------------------------------------");
    println!("Address: {address:?}");
    println!("----------------------------------------------------------------");
    println!("{build_params}");
}

pub async fn list_program_pdas(program_id: Address, client: &RpcClient) -> anyhow::Result<()> {
    let pdas = get_all_pdas_available(client, &program_id).await?;

    if pdas.is_empty() {
        println!("No verification PDAs found for program: {program_id}");
    } else {
        println!(
            "Found {} verification PDA(s) for program {program_id}:\n",
            pdas.len()
        );
        for (pda, build_params) in pdas {
            print_build_params(&pda, &build_params);
        }
    }

    Ok(())
}

pub async fn print_program_pda(
    program_id: Address,
    signer: Option<String>,
    client: &RpcClient,
    config_path: Option<String>,
) -> anyhow::Result<()> {
    let (pda, build_params) = get_program_pda(client, &program_id, signer, config_path).await?;
    print_build_params(&pda, &build_params);
    Ok(())
}

pub fn get_commit_hash(sub_m: &ArgMatches, repo_url: &str) -> anyhow::Result<String> {
    let commit_hash = sub_m
        .value_of("commit-hash")
        .map(String::from)
        .or_else(|| {
            get_commit_hash_from_remote(repo_url).ok() // Dynamically determine commit hash from remote
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Commit hash must be provided or inferred from the remote repository")
        })?;

    println!("Commit hash from remote: {commit_hash}");
    Ok(commit_hash)
}
