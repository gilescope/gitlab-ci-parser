use serde_derive::*;
use serde_yaml::{Mapping, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tracing::{debug, error, info, warn};
use yaml_merge_keys::merge_keys_serde;

pub type DynErr = Box<dyn std::error::Error + 'static>;

pub type StageName = String;
pub type JobName = String;
pub type VarName = String;
pub type VarValue = String;
pub type Script = String;

/// A job is a unit of work in gitlab. They can inherit from
/// multiple parents. A job consists of environment variables
/// and a list of commands to run.
/// All Jobs in the same stage tend to be run at once.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub stage: Option<StageName>,
    pub before_script: Option<Vec<Script>>,
    pub script: Option<Vec<Script>>,

    /// Even though variables could be None,
    /// they could be defined in extends_job.variables
    /// (or globally)
    pub variables: Option<BTreeMap<VarName, VarValue>>,

    pub extends: Option<Vec<JobName>>,

    /// You can alas extend more than one job.
    #[serde(skip)]
    pub extends_jobs: Vec<Rc<Job>>,
}

impl Job {
    /// Returns the consolidated local variables based on all extends.
    pub fn get_merged_variables(&self) -> BTreeMap<String, String> {
        let mut results = BTreeMap::new();
        self.calculate_variables(&mut results);
        results
    }

    fn calculate_variables(&self, mut variables: &mut BTreeMap<String, String>) {
        for parent in &self.extends_jobs {
            parent.calculate_variables(&mut variables);
        }
        if let Some(ref var) = self.variables {
            for (k, v) in var.iter() {
                variables.insert(k.clone(), v.clone());
            }
        }
    }
}

/// This is a parsed GitLabCI config.
/// GitLab is fairly imperative rather than declarative,
/// in that if `script:` is defined in the map before `variables:`
/// then any referenced variables are not set.
/// (Feels like a bug to me, but that's how gitlab seems to interpret this case)
#[derive(Debug)]
pub struct GitlabCIConfig {
    pub file: PathBuf,

    /// Based on include orderings, what's the parent of this gitlab config.
    pub parent: Option<Box<GitlabCIConfig>>,

    /// Global variables
    pub variables: BTreeMap<VarName, VarValue>,

    /// Stages group jobs that run in parallel. The ordering is important
    pub stages: Vec<StageName>,

    /// Targets that gitlab can run.
    pub jobs: BTreeMap<JobName, Rc<Job>>,
}

impl GitlabCIConfig {
    /// Returns the consolidated global variables based on all imports.
    pub fn get_merged_variables(&self) -> BTreeMap<String, String> {
        let mut results = BTreeMap::new();
        self.calculate_variables(&mut results);
        results
    }

    pub fn lookup_job(&self, job_name: &str) -> Option<Rc<Job>> {
        if let Some(job) = self.jobs.get(job_name) {
            Some(job.clone())
        } else {
            if let Some(parent) = &self.parent {
                parent.lookup_job(job_name)
            } else {
                None
            }
        }
    }

    fn calculate_variables(&self, mut variables: &mut BTreeMap<String, String>) {
        if let Some(ref parent) = self.parent {
            parent.calculate_variables(&mut variables);
        }
        variables.extend(self.variables.clone());
    }
}

//#[tracing::instrument]
fn parse_includes(
    context: &Path,
    include: &Value,
    parent: Option<GitlabCIConfig>,
) -> Option<GitlabCIConfig> {
    match include {
        Value::String(include_filename) => {
            // Remove leading '/' - join (correctly) won't concat them if filename starts from root.
            let ch = include_filename.chars().next().unwrap();
            let include_filename = if ch == '/' || ch == '\\' {
                include_filename[1..].to_owned()
            } else {
                include_filename.to_owned()
            };
            let include_filename = context.join(&include_filename);
            parse_aux(&context.join(&Path::new(&include_filename)), parent).ok()
        }
        Value::Sequence(includes) => {
            let mut parent = parent;
            for include in includes {
                parent = parse_includes(context, include, parent);
                if let Some(ref parent) = parent {
                    debug!("parent returned {:?}", &parent.file);
                } else {
                    error!(
                        "no parent found when parsing {:?} - is it checked out in a sister dir?",
                        &include
                    );
                }
            }
            parent
        }
        Value::Mapping(map) => {
            if let Some(Value::String(local)) = map.get(&Value::String("local".to_owned())) {
                let local = context.join(local);
                parse_aux(&local, parent).ok()
            } else if let Some(Value::String(project)) =
                map.get(&Value::String("project".to_owned()))
            {
                // We assume that the included project is checked out in a sister directory.
                let parts = project.split('/');
                let project_name = parts.last().expect("project name should contain '/'");

                if let Value::String(file) = map
                    .get(&Value::String("file".to_owned()))
                    .unwrap_or(&Value::String(".gitlab-ci.yml".to_owned()))
                {
                    let path = context.join(
                        Path::new("..")
                            .join(Path::new(project_name))
                            .join(Path::new(file)),
                    );
                    parse_aux(&path, parent).ok()
                } else {
                    parent
                }
            } else {
                parent
            }
        }
        _ => parent,
    }
}

///
/// Taking a path to a .gitlab-ci.yml file will read it and parse it where possible.
/// Anything unknown will be silently skipped. Jobs will be linked up with their parents.
///
pub fn parse(gitlab_file: &Path) -> Result<GitlabCIConfig, DynErr> {
    parse_aux(gitlab_file, None)
}

//#[tracing::instrument]
fn parse_aux(gitlab_file: &Path, parent: Option<GitlabCIConfig>) -> Result<GitlabCIConfig, DynErr> {
    debug!(
        "Parsing file {:?}, parent: {:?}",
        gitlab_file,
        parent.as_ref().map(|c| c.file.clone())
    );
    let f = std::fs::File::open(&gitlab_file)?;
    let raw_yaml = serde_yaml::from_reader(f)?;

    let val: serde_yaml::Value = merge_keys_serde(raw_yaml).expect("Couldn't merge yaml :<<");
    let mut config = GitlabCIConfig {
        file: gitlab_file.to_path_buf(),
        parent: None,
        stages: Vec::new(),
        variables: BTreeMap::new(),
        jobs: BTreeMap::new(),
    };

    if let serde_yaml::Value::Mapping(map) = val {
        info!("Parsing {:?} succesful.", gitlab_file);

        if let Some(includes) = map.get(&Value::String("include".to_owned())) {
            config.parent = parse_includes(
                gitlab_file
                    .parent()
                    .expect("gitlab-ci file wasn't in a dir??"),
                includes,
                parent,
            )
            .map(Box::new);
        } else {
            config.parent = parent.map(Box::new)
        }

        debug!(
            "All includes loaded for {:?}. {:?}",
            gitlab_file,
            config.parent.as_ref().map(|p| p.file.clone())
        );

        for (k, v) in map.iter() {
            if let Value::String(key) = k {
                if !config.jobs.contains_key(key) {
                    match (key.as_ref(), v) {
                        ("variables", _) => {
                            let global_var_map: Mapping = serde_yaml::from_value(v.clone())?;
                            for (key, value) in global_var_map {
                                if let (Value::String(key), Value::String(value)) = (key, value) {
                                    config.variables.insert(key, value);
                                }
                            }
                        }
                        ("stages", Value::Sequence(seq)) => {
                            for stage in seq {
                                if let Value::String(stage_name) = stage {
                                    config.stages.push(stage_name.to_owned());
                                }
                            }
                        }
                        (k, _) => {
                            let job_def = parse_job(&config, k, &map);
                            if let Ok(job) = job_def {
                                config.jobs.insert(k.to_owned(), job);
                            }
                        }
                    };
                }
            }
        }
    }

    Ok(config)
}

fn parse_value_as_string(val: &Value) -> Option<String> {
    match val {
        Value::String(ref result) => Some(result.clone()),
        _ => None,
    }
}

fn parse_value_as_strings(val: &Value) -> Option<Vec<String>> {
    match val {
        Value::String(result) => Some(vec![result.clone()]),
        Value::Sequence(results) => Some(
            results
                .iter()
                .map(parse_value_as_string)
                .filter_map(|o| o)
                .collect(),
        ),
        _ => None,
    }
}

fn parse_value_as_map(val: &Value) -> Option<BTreeMap<VarName, VarValue>> {
    match val {
        Value::Mapping(mapping) => {
            let mut res : BTreeMap<String, String> = BTreeMap::new();

            for (k, v) in mapping.iter() {
                match (k, v) {
                    (Value::String(k), Value::String(v)) => {
                        res.insert(k.into(), v.to_string());
                    }
                    (Value::String(k), Value::Number(v)) => {
                        res.insert(k.into(), v.to_string());
                    }
                    (Value::String(k), Value::Bool(v)) => {
                        res.insert(k.into(), v.to_string());
                    }
                    _ => {
                        warn!("didn't understand {:?}={:?}", k, v);
                    }
                };
            }

            Some(res)
        }
        _ => None,
    }
}

fn parse_raw_job(yml: &Mapping) -> Result<Job, DynErr> {
    let stage = yml
        .get(&Value::String("stage".into()))
        .map(parse_value_as_string)
        .unwrap_or(None);
    let before_script = yml
        .get(&Value::String("before_script".into()))
        .map(parse_value_as_strings)
        .unwrap_or(None);
    let script = yml
        .get(&Value::String("script".into()))
        .map(parse_value_as_strings)
        .unwrap_or(None);
    let extends = yml
        .get(&Value::String("extends".into()))
        .map(parse_value_as_strings)
        .unwrap_or(None);
    let variables = yml
        .get(&Value::String("variables".into()))
        .map(parse_value_as_map)
        .unwrap_or(None);

    Ok(Job {
        stage,
        before_script,
        script,
        variables,
        extends,
        extends_jobs: vec![],
    })
}

// When a file is loaded, all includes are imported, then all jobs, then
// only then do we load the jobs of the file that included us.
#[tracing::instrument]
fn parse_job(config: &GitlabCIConfig, job_name: &str, top: &Mapping) -> Result<Rc<Job>, DynErr> {
    let job_nm = Value::String(job_name.to_owned());
    if let Some(Value::Mapping(job)) = top.get(&job_nm) {
        let j: Result<Job, _> = parse_raw_job(job);
        if let Ok(mut j) = j {
            if let Some(ref parent_job_names) = j.extends {
                // Parse parents first so we don't get wicked fun with Rc<>...
                for parent_job_name in parent_job_names {
                    let job: Option<Rc<Job>> = if job_name != parent_job_name
                        && top.contains_key(&Value::String(parent_job_name.clone()))
                    {
                        parse_job(config, parent_job_name, top).ok()
                    } else {
                        config.lookup_job(parent_job_name)
                    };
                    if let Some(parent) = job {
                        j.extends_jobs.push(parent);
                    }
                }
            }
            Ok(Rc::new(j)) //TODO: maybe push rc outside here
        } else {
            Err(j.unwrap_err())
        }
    } else {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Job not found",
        )))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;
    use tracing::Level;
    use tracing_subscriber;

    #[test]
    pub fn parse_example() -> Result<(), DynErr> {
        let example_file: PathBuf = PathBuf::from(file!())
            .parent()
            .unwrap()
            .join("../examples/simple/.gitlab-ci.yml");

        // let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
        // let p = &PathBuf::from(Path::join(&root, "examples/simple/.gitlab-ci.yml"));
        let config = parse(&example_file)?;
        assert_eq!(
            config.variables["GLOBAL_VAR"],
            "this GLOBAL_VAR should mostly always be set.",
        );

        assert_eq!(config.stages.len(), 1);

        // Check jobs are linked up to their parents
        let job = config.jobs.get("tired_starlings").unwrap();

        let parent = job.extends_jobs[0].as_ref();
        assert!(parent
            .variables
            .as_ref()
            .unwrap()
            .contains_key("AN_INHERITED_VARIABLE"));
        Ok(())
    }

    #[test]
    pub fn parse_include() -> Result<(), DynErr> {
        let example_file: PathBuf = PathBuf::from(file!())
            .parent()
            .unwrap()
            .join("../.gitlab-ci.yml");

        let config = parse(&example_file)?;
        assert!(config.parent.is_some());

        let globals = config.get_merged_variables();
        assert!(globals.contains_key("GLOBAL_VAR"));
        Ok(())
    }

    #[test]
    pub fn consolidated_global_vars() -> Result<(), DynErr> {
        let example_file: PathBuf = PathBuf::from(file!())
            .parent()
            .unwrap()
            .join("../examples/simple/.gitlab-ci.yml");
        let config = parse(&example_file)?;
        let vars = config.get_merged_variables();
        assert!(vars.contains_key("GLOBAL_VAR"));
        Ok(())
    }

    #[test]
    pub fn imports() -> Result<(), DynErr> {
        let subscriber = tracing_subscriber::fmt()
            // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
            // will be written to stdout.
            .with_max_level(Level::TRACE)
            // builds the subscriber.
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let example_file: PathBuf = PathBuf::from(file!())
                .parent()
                .unwrap()
                .join("../examples/imports/a.yml");
            let config = parse(&example_file).unwrap();
            let vars = config.get_merged_variables();

            let mut parent = config.parent;
            println!("file {:?}", config.file);
            while let Some(par) = parent {
                println!("parent {:?}", par.file);
                parent = par.parent;
            }

            assert!(vars.contains_key("A"));
            assert!(vars.contains_key("B"));
            assert!(vars.contains_key("C"));
            assert!(vars.contains_key("D"));
            assert!(vars.contains_key("E"));
        });
        Ok(())
    }
}
