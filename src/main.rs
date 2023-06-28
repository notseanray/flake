use bollard::image::{BuildImageOptions, CreateImageOptions};
use bollard::models::EndpointSettings;
use bollard::network::ConnectNetworkOptions;
use bollard::network::CreateNetworkOptions;
use bollard::Docker;
use futures_util::{StreamExt, TryStreamExt};
use git2::build::{CheckoutBuilder, RepoBuilder};
use git2::{Cred, FetchOptions, Progress, RemoteCallbacks};
use serde::{Deserialize, Serialize};
use std::arch::asm;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::env::home_dir;
use std::error::Error;
use std::fs::{create_dir_all, read_to_string};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::thread::{self, panicking};
use std::time::Duration;
#[cfg(not(windows))]
use termion::async_stdin;
#[cfg(not(windows))]
use termion::raw::IntoRawMode;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::spawn;
use tokio::time::sleep;
use tokio_process_stream::ProcessLineStream;

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
struct ContainerNetwork {
    name: String,
    containers: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct MainConfig {
    repositories: Vec<Repo>,
    container_ordering: bool,
    containers: Vec<Container>,
    dashboard_port: Option<u16>,
    // Vec<name> of containers in network
    container_network: Option<ContainerNetwork>,
    worker_containers: Option<Vec<Container>>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct WorkerContainer {
    container: Container,
    // look at data directory
    data: Option<bool>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct Container {
    name: String,
    enabled: Option<bool>,
    env: Option<BTreeMap<String, String>>,
    async_execution: Option<bool>,
    output: bool,
    // two are mutually exclusive
    // fire after container
    depends: Option<Vec<String>>,
}

impl TryFrom<Container> for ContainerSetupJob {
    type Error = ();
    fn try_from(container: Container) -> Result<Self, Self::Error> {
        let entry = PathBuf::from(format!("configs/{}", container.name));
        if entry.is_dir() {
            let compose = entry.join("docker-compose.yml").exists();
            let docker_file = entry.join("Dockerfile").exists();
            let shell = entry.join(format!("{}.shell", container.name)).exists();
            if compose && docker_file {
                panic!("cannot have both docker compose and a docker file");
            }
            // TODO if multiple types, always run shell last
            let job_type = match (compose, docker_file, shell) {
                (true, false, false) => SetupFile::DockerCompose,
                (false, true, false) => SetupFile::DockerFile,
                (false, false, true) => SetupFile::Shell,
                _ => return Err(()),
            };
            return Ok(ContainerSetupJob {
                name: container.name.clone(),
                job_type,
                shell_hook: shell && (compose || docker_file),
                output: container.output,
            });
        }
        Err(())
    }
}

struct ContainerSetupJob {
    name: String,
    job_type: SetupFile,
    shell_hook: bool,
    // show output to stdout
    output: bool,
}

struct State {
    progress: Option<Progress<'static>>,
    total: usize,
    current: usize,
    path: Option<PathBuf>,
    newline: bool,
}

impl State {
    pub fn print_clone_status(&mut self) {
        let stats = self.progress.as_ref().unwrap();
        let network_pct = (100 * stats.received_objects()) / stats.total_objects();
        let index_pct = (100 * stats.indexed_objects()) / stats.total_objects();
        let kbytes = stats.received_bytes() / 1024;
        if stats.received_objects() == stats.total_objects() {
            if !self.newline {
                println!();
                self.newline = true;
            }
            print!(
                "Resolving deltas {}/{}\r",
                stats.indexed_deltas(),
                stats.total_deltas()
            );
        } else {
            print!(
                "\r{:3}% ({:4} kb, {:5}/{:5}) idx {:3}% ({:5}/{:5})",
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
        state.print_clone_status();
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
        state.print_clone_status();
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

#[derive(Debug)]
enum SetupFile {
    DockerCompose,
    DockerFile,
    Shell,
}

impl ContainerSetupJob {
    pub async fn execute(
        &self,
        docker: &Docker,
        main_config: Arc<MainConfig>,
    ) -> Result<(), Box<dyn Error>> {
        match self.job_type {
            SetupFile::Shell => {
                // TODO optimize this?
                let content = read_to_string(format!("configs/{}/{}.shell", self.name, self.name))?;
                for line in content.lines().filter(|x| !x.starts_with('#')) {
                    let mut line: Vec<&str> = line.split_whitespace().collect();
                    if line.is_empty() {
                        continue;
                    }
                    let mut cmd = Command::new(line.remove(0));
                    let process_dir = PathBuf::from(format!("./data/{}", &self.name));
                    cmd.current_dir(process_dir);
                    // if process_dir.exists() {
                    //     println!("exists");
                    // }
                    cmd.args(line);
                    let mut cmd = ProcessLineStream::try_from(cmd).expect("2");

                    let config = ConnectNetworkOptions {
                        container: &self.name,
                        endpoint_config: EndpointSettings::default(),
                    };

                    if let Some(v) = &main_config.container_network {
                        let _ = docker.connect_network(&v.name, config).await;
                    }

                    while let Some(v) = cmd.next().await {
                        println!("[{}] {}", self.name, v);
                    }
                }
            }
            SetupFile::DockerFile => {
                let build_options = BuildImageOptions {
                    dockerfile: format!("configs/{}/Dockerfile", self.name),
                    networkmode: "bridge".to_string(),
                    ..BuildImageOptions::default()
                };
                let mut build_options = docker.build_image(build_options, None, None);

                let config = ConnectNetworkOptions {
                    container: &self.name,
                    endpoint_config: EndpointSettings::default(),
                };

                if let Some(v) = &main_config.container_network {
                    let _ = docker.connect_network(&v.name, config).await;
                }

                while let Some(msg) = build_options.next().await {
                    println!("[{}] {:?}", self.name, msg);
                }
            }
            SetupFile::DockerCompose => {
                let mut compose_cmd = Command::new("docker-compose");
                compose_cmd.args([
                    "-f",
                    format!("configs/{}/docker-compose.yml", self.name).as_str(),
                    "up",
                    "-d",
                ]);
                let mut process = ProcessLineStream::try_from(compose_cmd).expect("`");

                let config = ConnectNetworkOptions {
                    container: &self.name,
                    endpoint_config: EndpointSettings::default(),
                };

                if let Some(v) = &main_config.container_network {
                    let _ = docker.connect_network(&v.name, config).await;
                }

                while let Some(v) = process.next().await {
                    println!("[{}] {}", self.name, v);
                }
            }
        };
        Ok(())
    }

    pub async fn execute_threaded(
        &self,
        docker: Arc<Docker>,
        main_config: Arc<MainConfig>,
    ) -> Result<(), Box<dyn Error>> {
        match self.job_type {
            SetupFile::Shell => {
                // TODO optimize this?
                let content = read_to_string(format!("configs/{}/{}.shell", self.name, self.name))?;
                for line in content.lines().filter(|x| !x.starts_with('#')) {
                    let mut line: Vec<&str> = line.split_whitespace().collect();
                    if line.is_empty() {
                        continue;
                    }
                    let mut cmd = Command::new(line.remove(0));
                    let process_dir = PathBuf::from(format!("./data/{}", &self.name));
                    cmd.current_dir(process_dir);
                    // if process_dir.exists() {
                    //     println!("exists");
                    // }
                    cmd.args(line);
                    let mut cmd = ProcessLineStream::try_from(cmd).expect("2");

                    let config = ConnectNetworkOptions {
                        container: &self.name,
                        endpoint_config: EndpointSettings::default(),
                    };

                    if let Some(v) = &main_config.container_network {
                        let _ = docker.connect_network(&v.name, config).await;
                    }

                    while let Some(v) = cmd.next().await {
                        println!("[{}] {}", self.name, v);
                    }
                }
            }
            SetupFile::DockerFile => {
                let build_options = BuildImageOptions {
                    dockerfile: format!("configs/{}/Dockerfile", self.name),
                    networkmode: "bridge".to_string(),
                    ..BuildImageOptions::default()
                };
                let mut build_options = docker.build_image(build_options, None, None);

                let config = ConnectNetworkOptions {
                    container: &self.name,
                    endpoint_config: EndpointSettings::default(),
                };

                if let Some(v) = &main_config.container_network {
                    let _ = docker.connect_network(&v.name, config).await;
                }

                while let Some(msg) = build_options.next().await {
                    println!("[{}] {:?}", self.name, msg);
                }
            }
            SetupFile::DockerCompose => {
                let mut compose_cmd = Command::new("docker-compose");
                compose_cmd.args([
                    "-f",
                    format!("configs/{}/docker-compose.yml", self.name).as_str(),
                    "up",
                    "-d",
                ]);
                let mut process = ProcessLineStream::try_from(compose_cmd).expect("`");

                let config = ConnectNetworkOptions {
                    container: &self.name,
                    endpoint_config: EndpointSettings::default(),
                };

                if let Some(v) = &main_config.container_network {
                    let _ = docker.connect_network(&v.name, config).await;
                }

                while let Some(v) = process.next().await {
                    println!("[{}] {}", self.name, v);
                }
            }
        };
        Ok(())
    }
}

// TODO check if containers are already running for custom configs, if so prompt to kill
// CLAP arg parser and config helper (generate default config)
// separate docker files for this CI in repo
// logging crate
// worker container to perform some basic functions like set things up
// datalake
// datalake

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
    // validate config
    if !PathBuf::from("configs").exists() {
        println!("no config folder exists, exiting");
        std::process::exit(1);
    }
    let config = read_to_string("configs/main.yml")?;
    let main_config: Arc<MainConfig> = Arc::new(serde_yaml::from_str(&config)?);

    create_dir_all("data")?;

    for repository in &main_config.repositories {
        let dir = format!("./data/{}", repository.name);
        if PathBuf::from(&dir).exists() {
            continue;
        }
        clone_repository(&repository.url, &dir)?;
    }

    let docker = Docker::connect_with_socket_defaults()?;
    if let Some(v) = &main_config.container_network {
        let config = CreateNetworkOptions {
            name: v.name.clone(),
            attachable: true,
            ingress: true,
            driver: "overlay".to_string(),
            ..Default::default()
        };
        // let _ = docker.create_network(config).await;
    }
    // this is the turbo based thread scheduler, don't ask how it works because i don't remember
    // i think i wrote it down on a sticky note
    //
    // first, iterate through all the containers
    // if they don't have deps then just stick them in the front
    // if they do have deps but they're already in the list to execute then insert at the index of
    // the last dep/latest index
    // otherwise insert at the back
    // go through a pass to try and find if any have circular deps
    // maintain a list of jobs that are complete (Arc<RwLock<Vec<String>>>)
    // if the job has no dep, execute it in parallel
    // if the job has deps, constantly poll the list to see if all deps are complete
    // if they are execute them
    if !main_config.container_ordering {
        let mut scheduled: Vec<Container> = Vec::with_capacity(main_config.containers.len());
        for container in &main_config.containers {
            if let Some(deps) = &container.depends {
                let mut fufilled_list: Vec<Option<usize>> = deps
                    .iter()
                    .map(|x| scheduled.iter().position(|p| &p.name == x))
                    .collect();
                fufilled_list.sort_unstable_by(|l, r| {
                    if let (Some(l), Some(r)) = (l, r) {
                        l.partial_cmp(r).unwrap()
                    } else {
                        Ordering::Less
                    }
                });
                if !fufilled_list.contains(&None) {
                    scheduled.push(container.clone());
                    continue;
                }
                if let Some(Some(v)) = fufilled_list.last() {
                    scheduled.insert(*v, container.clone());
                    continue;
                }
                scheduled.push(container.clone());
                continue;
            }
            scheduled.insert(0, container.clone());
        }
        // detect any circular deps
        for container in &main_config.containers {
            if let Some(dependencies) = &container.depends {
                if dependencies.contains(&container.name) {
                    // exit here
                    panic!("{} depends on itself", container.name);
                }
                for depend in dependencies {
                    // look at each dependancy
                    if let Some(dep_container) = main_config
                        .containers
                        .iter()
                        .filter(|x| &x.name == depend)
                        .collect::<Vec<&Container>>()
                        .pop()
                    {
                        if let Some(deps) = &dep_container.depends {
                            if deps.contains(&container.name) {
                                panic!(
                                    "[CIRCULAR DEPENDANCY] {} depends on {} which depends on {}",
                                    container.name, dep_container.name, container.name
                                );
                            }
                        }
                    }
                }
            }
        }
        let docker = Arc::new(docker);
        let container_setup_queue: Vec<(Container, ContainerSetupJob)> = scheduled
            .iter()
            .filter_map(|x| x.clone().try_into().ok().map(|v| (x.clone(), v)))
            .collect();
        let finished_jobs = Arc::new(RwLock::new(Vec::with_capacity(container_setup_queue.len())));
        for (c, j) in container_setup_queue {
            let main_config = main_config.clone();
            let docker = docker.clone();
            let finished_jobs = finished_jobs.clone();
            tokio::spawn(async move {
                // delay until all dependants are finished
                if let Some(v) = c.depends {
                    // prevent release mode from determining that the loop will execute 0 times and
                    // removing the whole branch
                    unsafe {
                        asm!("nop");
                    }
                    while !v
                        .iter()
                        .map(|x| finished_jobs.read().unwrap().contains(x))
                        .all(|x| x)
                    {
                        // actually the most brain dead thing ever, force the compiler to not
                        // create a branch without the loop so it actually waits for deps to finish
                        //
                        // "sometimes my genius is almost frightening"
                        unsafe {
                            asm!("nop");
                        }
                    }
                }
                j.execute_threaded(docker.clone(), main_config)
                    .await
                    .unwrap();
                finished_jobs.write().unwrap().push(c.name.clone());
            });
        }
        // }
    } else {
        let container_setup_queue: Vec<ContainerSetupJob> = main_config
            .containers
            .iter()
            .filter_map(|x| x.clone().try_into().ok())
            .collect();
        // non par
        for c in container_setup_queue {
            c.execute(&docker, main_config.clone()).await?;
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

    // docker
    //     .create_image(
    //         Some(CreateImageOptions {
    //             from_image: "",
    //             ..Default::default()
    //         }),
    //         None,
    //         None,
    //     )
    //     .try_collect::<Vec<_>>()
    //     .await?;
    // let alpine_config = Config {
    //     image: Some(""),
    //     tty: Some(true),
    //     attach_stdin: Some(true),
    //     attach_stdout: Some(true),
    //     attach_stderr: Some(true),
    //     open_stdin: Some(true),
    //     ..Default::default()
    // };
    // let id = docker
    //     .create_container::<&str, &str>(None, alpine_config)
    //     .await?
    //     .id;
    //
    // docker.start_container::<String>(&id, None).await?;
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
    // #[cfg(not(windows))]
    // if false {
    //     let AttachContainerResults {
    //         mut output,
    //         mut input,
    //     } = docker
    //         .attach_container(
    //             &id,
    //             Some(AttachContainerOptions::<String> {
    //                 stdout: Some(true),
    //                 stderr: Some(true),
    //                 stdin: Some(true),
    //                 stream: Some(true),
    //                 ..Default::default()
    //             }),
    //         )
    //         .await?;
    //
    //     // pipe stdin into the docker attach stream input
    //     spawn(async move {
    //         let mut stdin = async_stdin().bytes();
    //         loop {
    //             if let Some(Ok(byte)) = stdin.next() {
    //                 input.write(&[byte]).await.ok();
    //             } else {
    //                 sleep(Duration::from_nanos(10)).await;
    //             }
    //         }
    //     });
    //
    //     // set stdout in raw mode so we can do tty stuff
    //     let stdout = stdout();
    //     let mut stdout = stdout.lock().into_raw_mode()?;
    //
    //     // pipe docker attach output into stdout
    //     while let Some(Ok(output)) = output.next().await {
    //         stdout.write_all(output.into_bytes().as_ref())?;
    //         stdout.flush()?;
    //     }
    // }
    // sleep(Duration::from_secs(30));
    // docker
    //     .remove_container(
    //         &id,
    //         Some(RemoveContainerOptions {
    //             force: true,
    //             ..Default::default()
    //         }),
    //     )
    //     .await?;
    println!("Done");
    thread::sleep(Duration::MAX);
    Ok(())
}
