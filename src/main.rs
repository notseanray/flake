use bollard::exec::{CreateExecOptions, StartExecResults};
use git2::build::{CheckoutBuilder, RepoBuilder};
use git2::{Cred, FetchOptions, Progress, RemoteCallbacks};
use serde::{Deserialize, Serialize};
use std::env::home_dir;

use bollard::container::{
    AttachContainerOptions, AttachContainerResults, Config, RemoveContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use futures_util::{StreamExt, TryStreamExt};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::{create_dir_all, read_dir, read_to_string, ReadDir};
use std::io::{stdout, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(not(windows))]
use termion::async_stdin;
#[cfg(not(windows))]
use termion::raw::IntoRawMode;
use tokio::io::AsyncWriteExt;
use tokio::task::spawn;
use tokio::time::sleep;

struct LoggingSystem {
    source_container: String,
    // maybe change this
    timestamp: u64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Repo {
    name: String,
    url: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct MainConfig {
    clone_ordering: bool,
    repositories: Vec<Repo>,
    container_ordering: bool,
    containers: Vec<Container>,
    dashboard_port: Option<u16>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Container {
    name: String,
    env: Option<BTreeMap<String, String>>,
}

struct State {
    progress: Option<Progress<'static>>,
    total: usize,
    current: usize,
    path: Option<PathBuf>,
    newline: bool,
}

fn print_clone_status(state: &mut State) {
    let stats = state.progress.as_ref().unwrap();
    let network_pct = (100 * stats.received_objects()) / stats.total_objects();
    let index_pct = (100 * stats.indexed_objects()) / stats.total_objects();
    let kbytes = stats.received_bytes() / 1024;
    if stats.received_objects() == stats.total_objects() {
        if !state.newline {
            println!();
            state.newline = true;
        }
        print!(
            "Resolving deltas {}/{}\r",
            stats.indexed_deltas(),
            stats.total_deltas()
        );
    } else {
        print!(
            "{:3}% ({:4} kb, {:5}/{:5}) idx {:3}% ({:5}/{:5})\r",
            network_pct,
            kbytes,
            stats.received_objects(),
            stats.total_objects(),
            index_pct,
            stats.indexed_objects(),
            stats.total_objects(),
        )
    }
    std::io::stdout().flush().unwrap();
}

// TODO improve this
fn get_key_path(extension: Option<&str>) -> PathBuf {
    // let ssh_key = env::var("GIT_SSH_KEY_NAME")
    // 	.expect("No GIT_SSH_KEY_NAME environment variable found");

    // TODO replace with crate
    let mut path = home_dir().unwrap();
    path.push(".ssh");
    path.push("id_ed25519");

    if let Some(extension_value) = extension {
        path.set_extension(extension_value);
    }

    path
}

fn clone_repository(url: &str, path: &str) -> Result<(), git2::Error> {
    let state = RefCell::new(State {
        progress: None,
        total: 0,
        current: 0,
        path: None,
        newline: false,
    });
    let mut cb = RemoteCallbacks::new();
    cb.transfer_progress(|stats| {
        let mut state = state.borrow_mut();
        state.progress = Some(stats.to_owned());
        print_clone_status(&mut state);
        true
    });
    cb.credentials(
        |_url: &str, username: Option<&str>, _cred_type: git2::CredentialType| {
            let public = get_key_path(Some("pub"));
            let private = get_key_path(None);

            return Cred::ssh_key(
                username.unwrap(),
                Some(public.as_path()),
                private.as_path(),
                None,
            );
        },
    );

    let mut co = CheckoutBuilder::new();
    co.progress(|path, cur, total| {
        let mut state = state.borrow_mut();
        state.path = path.map(|p| p.to_path_buf());
        state.current = cur;
        state.total = total;
        print_clone_status(&mut state);
    });

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(cb);
    RepoBuilder::new()
        .fetch_options(fo)
        .with_checkout(co)
        .clone(url, Path::new(path))?;
    println!();

    Ok(())
}

enum ContainerSetupType {
    DockerCompose,
    DockerFile,
    Shell
}

struct ContainerSetupJob {
    pub name: String,
    pub job_type: ContainerSetupType,
    pub shell: bool,
    // show output to stdout
    pub output: bool,
}

impl ContainerSetupJob {
    pub fn execute(self) {

    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
    // validate config
    if !PathBuf::from("configs").exists() {
        println!("no config folder exists, exiting");
        std::process::exit(1);
    }
    let main_config: MainConfig = serde_yaml::from_str(&read_to_string("configs/main.yml")?)?;

    create_dir_all("data")?;

    for repository in main_config.repositories {
        let dir = format!("./data/{}", repository.name);
        if PathBuf::from(&dir).exists() {
            continue;
        }
        clone_repository(&repository.url, &dir)?;
    }

    let container_setup_queue = Vec::with_capacity(main_config.containers.len());

    for container in main_config.containers {
        let entry = PathBuf::from(format!("configs/{}", container.name));
        if entry.is_dir() {
            let compose = entry.join("docker-compose.yml").exists();
            let docker_file = entry.join("Dockerfile").exists();
            let shell = entry.join(".shell").exists();
            if compose && docker_file {
                panic!("cannot have both docker compose and a docker file");
            }
            let job_type = match 
            ContainerSetupJob {
                name: container.name,
                job_type,
                shell: true,
                output: true,
            }
            continue;
        }
    }
    // just a file -> raw shell
    // dockerfile try to parse as yaml, if fail then docker file
    // if directory compose file
    // test are specific folder
    // for entry in read_dir("configs")?.filter_map(|x| x.ok()) {
    //     if entry.file_type()?.is_dir() {
    //         match entry.file_name().as_os_str().to_str().unwrap_or_default() {
    //             "tests" => {
    //
    //             }
    //             _ => {}
    //         }
    //         for file in read_dir(entry.path())?.filter_map(|x| x.ok()) {
    //
    //         }
    //         continue;
    //     }
    //
    // }
    panic!("test");
    let docker = Docker::connect_with_socket_defaults().unwrap();

    docker
        .create_image(
            Some(CreateImageOptions {
                from_image: "",
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await?;
    let alpine_config = Config {
        image: Some(""),
        tty: Some(true),
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        open_stdin: Some(true),
        ..Default::default()
    };
    let id = docker
        .create_container::<&str, &str>(None, alpine_config)
        .await?
        .id;

    docker.start_container::<String>(&id, None).await?;
    // for cmd in setup {
    // non interactive
    //     let exec = docker
    //         .create_exec(
    //             &id,
    //             CreateExecOptions {
    //                 attach_stdout: Some(true),
    //                 attach_stderr: Some(true),
    //                 cmd: Some(cmd),
    //                 ..Default::default()
    //             },
    //         )
    //         .await?
    //         .id;
    //     if let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec, None).await? {
    //         while let Some(Ok(msg)) = output.next().await {
    //             print!("{}", msg);
    //         }
    //     } else {
    //         unreachable!();
    //     }
    // }
    //
    #[cfg(not(windows))]
    if false {
        let AttachContainerResults {
            mut output,
            mut input,
        } = docker
            .attach_container(
                &id,
                Some(AttachContainerOptions::<String> {
                    stdout: Some(true),
                    stderr: Some(true),
                    stdin: Some(true),
                    stream: Some(true),
                    ..Default::default()
                }),
            )
            .await?;

        // pipe stdin into the docker attach stream input
        spawn(async move {
            let mut stdin = async_stdin().bytes();
            loop {
                if let Some(Ok(byte)) = stdin.next() {
                    input.write(&[byte]).await.ok();
                } else {
                    sleep(Duration::from_nanos(10)).await;
                }
            }
        });

        // set stdout in raw mode so we can do tty stuff
        let stdout = stdout();
        let mut stdout = stdout.lock().into_raw_mode()?;

        // pipe docker attach output into stdout
        while let Some(Ok(output)) = output.next().await {
            stdout.write_all(output.into_bytes().as_ref())?;
            stdout.flush()?;
        }
    }
    sleep(Duration::from_secs(30));
    docker
        .remove_container(
            &id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}
