use bollard::service::Volume;
use serde::{Serialize, Deserialize};
use bollard::exec::{CreateExecOptions, StartExecResults};

use bollard::container::{
    AttachContainerOptions, AttachContainerResults, Config, RemoveContainerOptions,
};
use bollard::Docker;
use docker_compose_types::Compose;
use bollard::image::CreateImageOptions;
use futures_util::{StreamExt, TryStreamExt};
use std::collections::{BTreeMap, HashMap};
use std::fs::{ReadDir, read_dir};
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

const ALPINE_IMAGE: &str = "alpine:3";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum ContainerConfig {
    //  raw docker file
    DockerFile(Box<Path>),
    // compose
    DockerComposeFile(Box<Path>),
    // text file
    ManualConfig(Container),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Container {
    from: String,
    env: BTreeMap<String, String>,
    // events:
    // expose at end
    ports: Vec<u16>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct RootContainer {
    subcontainers: Vec<ContainerConfig>,
    ports: Vec<u16>,
}

fn parse_configs<P: AsRef<Path>>(dir: P) -> Result<(), Box<dyn std::error::Error + 'static>> {
    for dir in read_dir(dir)?.filter_map(|x| x.ok()) {
        if dir.file_type()?.is_dir() {
            parse_configs(dir.path())?;
            continue;
        }
        ContainerConfig
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
    if !PathBuf::from("config").exists() {
        println!("no config folder exists, existing");
        std::process::exit(1);
    }
    //apk update && apk add --no-cache docker-cli
    //docker pull localstack/localstack
    let setup = vec![
        // vec!["apk", "update", "&&", "apk", "add", "--no-cache", "docker-cli", "docker", "docker-compose"],
        vec!["docker", "pull", "localstack/localstack"],
        vec!["docker", "pull", "mongo"],
        vec!["docker", "pull", "mcr.microsoft.com/playwright:v1.35.0-jammy"],
    ];
    let docker = Docker::connect_with_socket_defaults().unwrap();


    docker
        .create_image(
            Some(CreateImageOptions {
                from_image: IMAGE,
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await?;
    let alpine_config = Config {
        image: Some(IMAGE),
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
