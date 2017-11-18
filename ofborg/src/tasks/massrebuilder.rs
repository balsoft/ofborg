extern crate amqp;
extern crate env_logger;

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use ofborg::checkout;
use ofborg::message::massrebuildjob;
use ofborg::nix;

use ofborg::worker;
use amqp::protocol::basic::{Deliver,BasicProperties};
use hubcaps;

pub struct MassRebuildWorker {
    cloner: checkout::CachedCloner,
    nix: nix::Nix,
    github: hubcaps::Github,
}

impl MassRebuildWorker {
    pub fn new(cloner: checkout::CachedCloner, nix: nix::Nix, github: hubcaps::Github) -> MassRebuildWorker {
        return MassRebuildWorker{
            cloner: cloner,
            nix: nix,
            github: github,
        };
    }

    fn actions(&self) -> massrebuildjob::Actions {
        return massrebuildjob::Actions{
        };
    }
}

impl worker::SimpleWorker for MassRebuildWorker {
    type J = massrebuildjob::MassRebuildJob;

    fn msg_to_job(&self, _: &Deliver, _: &BasicProperties,
                  body: &Vec<u8>) -> Result<Self::J, String> {
        return match massrebuildjob::from(body) {
            Ok(e) => { Ok(e) }
            Err(e) => {
                println!("{:?}", String::from_utf8(body.clone()));
                panic!("{:?}", e);
            }
        }
    }

    fn consumer(&self, job: &massrebuildjob::MassRebuildJob) -> worker::Actions {
        let repo = self.github
            .repo(job.repo.owner.clone(), job.repo.name.clone());
        let gists = self.github.gists();


        let mut overall_status = CommitStatus::new(
            repo.statuses(),
            job.pr.head_sha.clone(),
            "grahamcofborg-eval".to_owned(),
            "Starting".to_owned(),
            None
        );

        overall_status.set_with_description("Starting", hubcaps::statuses::State::Pending);

        let project = self.cloner.project(job.repo.full_name.clone(), job.repo.clone_url.clone());

        overall_status.set_with_description("Cloning project", hubcaps::statuses::State::Pending);

        let co = project.clone_for("mr-est".to_string(),
                                   job.pr.number.to_string()).unwrap();

        let target_branch = match job.pr.target_branch.clone() {
            Some(x) => { x }
            None => { String::from("origin/master") }
        };

        overall_status.set_with_description(
            format!("Checking out {}", target_branch),
            hubcaps::statuses::State::Pending
        );
        let refpath = co.checkout_ref(target_branch.as_ref()).unwrap();


        overall_status.set_with_description(
            "Checking original stdenvs",
            hubcaps::statuses::State::Pending
        );

        let mut stdenvs = Stdenvs::new(self.nix.clone(), PathBuf::from(&refpath));
        stdenvs.identify_before();

        overall_status.set_with_description(
            "Fetching PR",
            hubcaps::statuses::State::Pending
        );

        co.fetch_pr(job.pr.number).unwrap();

        if !co.commit_exists(job.pr.head_sha.as_ref()) {
            overall_status.set_with_description(
                "Commit not found",
                hubcaps::statuses::State::Error
            );

            info!("Commit {} doesn't exist", job.pr.head_sha);
            return self.actions().skip(&job);
        }

        overall_status.set_with_description(
            "Merging PR",
            hubcaps::statuses::State::Pending
        );

        if let Err(_) = co.merge_commit(job.pr.head_sha.as_ref()) {
            overall_status.set_with_description(
                "Failed to merge",
                hubcaps::statuses::State::Failure
            );

            info!("Failed to merge {}", job.pr.head_sha);
            return self.actions().skip(&job);
        }

        overall_status.set_with_description(
            "Checking new stdenvs",
            hubcaps::statuses::State::Pending
        );

        stdenvs.identify_after();

        println!("Got path: {:?}, building", refpath);
        overall_status.set_with_description(
            "Begining Evaluations",
            hubcaps::statuses::State::Pending
        );

        let eval_checks = vec![
            EvalChecker::new("package-list",
                             "nix-env",
                             vec![
                                 String::from("--file"),
                                 String::from("."),
                                 String::from("--query"),
                                 String::from("--available"),
                                 String::from("--json"),
                             ],
                             self.nix.clone()
            ),

            EvalChecker::new("nixos-options",
                             "nix-instantiate",
                             vec![
                                 String::from("./nixos/release.nix"),
                                 String::from("-A"),
                                 String::from("options"),
                             ],
                             self.nix.clone()
            ),

            EvalChecker::new("nixos-manual",
                             "nix-instantiate",
                             vec![
                                 String::from("./nixos/release.nix"),
                                 String::from("-A"),
                                 String::from("manual"),
                             ],
                             self.nix.clone()
            ),

            EvalChecker::new("nixpkgs-manual",
                             "nix-instantiate",
                             vec![
                                 String::from("./pkgs/top-level/release.nix"),
                                 String::from("-A"),
                                 String::from("manual"),
                             ],
                             self.nix.clone()
            ),

            EvalChecker::new("nixpkgs-tarball",
                             "nix-instantiate",
                             vec![
                                 String::from("./pkgs/top-level/release.nix"),
                                 String::from("-A"),
                                 String::from("tarball"),
                             ],
                             self.nix.clone()
            ),

            EvalChecker::new("nixpkgs-unstable-jobset",
                             "nix-instantiate",
                             vec![
                                 String::from("./pkgs/top-level/release.nix"),
                                 String::from("-A"),
                                 String::from("unstable"),
                             ],
                             self.nix.clone()
            ),
        ];

        let eval_results: bool = eval_checks.into_iter()
            .map(|check|
                 {
                     let mut status = CommitStatus::new(
                         repo.statuses(),
                         job.pr.head_sha.clone(),
                         check.name(),
                         check.cli_cmd(),
                         None
                     );

                     status.set(hubcaps::statuses::State::Pending);

                     let state: hubcaps::statuses::State;
                     let mut out: File;
                     match check.execute((&refpath).to_owned()) {
                         Ok(o) => {
                             out = o;
                             state = hubcaps::statuses::State::Success;
                         }
                         Err(o) => {
                             out = o;
                             state = hubcaps::statuses::State::Failure;
                         }
                     }

                     let mut files = HashMap::new();
                     files.insert(check.name(),
                                  hubcaps::gists::Content {
                                      filename: Some(check.name()),
                                      content: file_to_str(&mut out),
                                  }
                     );

                     let gist_url = gists.create(
                         &hubcaps::gists::GistOptions {
                             description: Some(format!("{:?}", state)),
                             public: Some(true),
                             files: files,
                         }
                     ).expect("Failed to create gist!").html_url;

                     status.set_url(Some(gist_url));
                     status.set(state.clone());

                     if state == hubcaps::statuses::State::Success {
                         return Ok(())
                     } else {
                         return Err(())
                     }
                 }
            )
            .all(|status| status == Ok(()));

        if eval_results {
            overall_status.set_with_description(
                "Calculating Changed Outputs",
                hubcaps::statuses::State::Pending
            );

            if !stdenvs.are_same() {
                println!("Stdenvs changed? {:?}", stdenvs.changed());
            }


        }

        return vec![];
    }
}

struct CommitStatus<'a> {
    api: hubcaps::statuses::Statuses<'a>,
    sha: String,
    context: String,
    description: String,
    url: String,
}

impl <'a> CommitStatus<'a> {
    fn new(api: hubcaps::statuses::Statuses<'a>, sha: String, context: String, description: String, url: Option<String>) -> CommitStatus<'a> {
        let mut stat = CommitStatus {
            api: api,
            sha: sha,
            context: context,
            description: description,
            url: "".to_owned(),
        };

        stat.set_url(url);

        return stat
    }

    fn set_url(&mut self, url: Option<String>) {
        self.url = url.unwrap_or(String::from(""))
    }

    fn set_with_description(&mut self, description: &str, state: hubcaps::statuses::State) {
        self.set_description(description.to_owned());
        self.set(state);
    }

    fn set_description(&mut self, description: String) {
        self.description = description;
    }

    fn set(&self, state: hubcaps::statuses::State) {
        self.api.create(
            self.sha.as_ref(),
            &hubcaps::statuses::StatusOptions::builder(state)
                .context(self.context.clone())
                .description(self.description.clone())
                .target_url(self.url.clone())
                .build()
        ).expect("Failed to mark final status on commit");
    }
}

struct EvalChecker {
    name: String,
    cmd: String,
    args: Vec<String>,
    nix: nix::Nix,

}

impl EvalChecker {
    fn new(name: &str, cmd: &str, args: Vec<String>, nix: nix::Nix) -> EvalChecker {
        EvalChecker{
            name: name.to_owned(),
            cmd: cmd.to_owned(),
            args: args,
            nix: nix,
        }
    }

    fn name(&self) -> String {
        format!("grahamcofborg-eval-{}", self.name)
    }

    fn execute(&self, path: String) -> Result<File, File> {
        self.nix.safely(&self.cmd, &Path::new(&path), self.args.clone())
    }

    fn cli_cmd(&self) -> String {
        let mut cli = vec![self.cmd.clone()];
        cli.append(&mut self.args.clone());
        return cli.join(" ");
    }
}

enum StdenvFrom {
    Before,
    After
}

#[derive(Debug)]
enum System {
    X8664Darwin,
    X8664Linux
}

#[derive(Debug, PartialEq)]
struct Stdenvs {
    nix: nix::Nix,
    co: PathBuf,

    linux_stdenv_before: Option<String>,
    linux_stdenv_after: Option<String>,

    darwin_stdenv_before: Option<String>,
    darwin_stdenv_after: Option<String>,
}

impl Stdenvs {
    fn new(nix: nix::Nix, co: PathBuf) -> Stdenvs {
        return Stdenvs {
            nix: nix,
            co: co,

            linux_stdenv_before: None,
            linux_stdenv_after: None,

            darwin_stdenv_before: None,
            darwin_stdenv_after: None,
        }
    }

    fn identify_before(&mut self) {
        self.identify(System::X8664Linux, StdenvFrom::Before);
        self.identify(System::X8664Darwin, StdenvFrom::Before);
    }

    fn identify_after(&mut self) {
        self.identify(System::X8664Linux, StdenvFrom::After);
        self.identify(System::X8664Darwin, StdenvFrom::After);
    }

    fn are_same(&self) -> bool {
        return self.changed().len() == 0;
    }

    fn changed(&self) -> Vec<System> {
        let mut changed: Vec<System> = vec![];

        if self.linux_stdenv_before != self.linux_stdenv_after {
            changed.push(System::X8664Linux);
        }

        if self.darwin_stdenv_before != self.darwin_stdenv_after {
            changed.push(System::X8664Darwin);
        }


        return changed
    }

    fn identify(&mut self, system: System, from: StdenvFrom) {
        match (system, from) {
            (System::X8664Linux, StdenvFrom::Before) => {
                self.linux_stdenv_before = self.evalstdenv("x86_64-linux");
            }
            (System::X8664Linux, StdenvFrom::After) => {
                self.linux_stdenv_after = self.evalstdenv("x86_64-linux");
            }

            (System::X8664Darwin, StdenvFrom::Before) => {
                self.darwin_stdenv_before = self.evalstdenv("x86_64-darwin");
            }
            (System::X8664Darwin, StdenvFrom::After) => {
                self.darwin_stdenv_after = self.evalstdenv("x86_64-darwin");
            }
        }
    }

    fn evalstdenv(&self, system: &str) -> Option<String> {
        let result = self.nix.with_system(system.to_owned()).safely(
            "nix-instantiate", &self.co, vec![
                String::from("."),
                String::from("-A"),
                String::from("stdenv"),
            ]
        );

        println!("{:?}", result);

        return match result {
            Ok(mut out) => {
                file_to_drv(&mut out)
            }
            Err(mut out) => {
                println!("{:?}", file_to_str(&mut out));
                None
            }
        }
    }
}

fn file_to_drv(f: &mut File) -> Option<String> {
    let r = BufReader::new(f);
    let matches: Vec<String>;
    matches = r.lines().filter_map(|x|
                     match x {
                         Ok(line) => {
                             if !line.starts_with("/nix/store/") {
                                 debug!("Skipping line, not /nix/store: {}", line);
                                 return None
                             }

                             if !line.ends_with(".drv") {
                                 debug!("Skipping line, not .drv: {}", line);
                                 return None
                             }

                             return Some(line)
                         }
                         Err(_) => None
                     }).collect();

    if matches.len() == 1 {
        return Some(matches.first().unwrap().clone());
    } else {
        info!("Got wrong number of matches: {}", matches.len());
        info!("Matches: {:?}", matches);
        return None
    }
}

fn file_to_str(f: &mut File) -> String {
    let mut buffer = Vec::new();
    f.read_to_end(&mut buffer).expect("Reading eval output");
    return String::from(String::from_utf8_lossy(&buffer));
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn stdenv_checking() {
        let nix = nix::new(String::from("x86_64-linux"), String::from("daemon"));
        let mut stdenv = Stdenvs::new(nix.clone(), PathBuf::from("/nix/var/nix/profiles/per-user/root/channels/nixos/nixpkgs"));
        stdenv.identify(System::X8664Linux, StdenvFrom::Before);
        stdenv.identify(System::X8664Darwin, StdenvFrom::Before);

        stdenv.identify(System::X8664Linux, StdenvFrom::After);
        stdenv.identify(System::X8664Darwin, StdenvFrom::After);

        assert!(stdenv.are_same());
    }
}
