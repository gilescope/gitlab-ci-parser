use serde_derive::*;
use serde_yaml::{Mapping, Value};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use yaml_merge_keys::merge_keys_serde;

pub type DynErr = Box<dyn std::error::Error + 'static>;

pub type StageName = String;
pub type JobName = String;
pub type VarName = String;
pub type VarValue = String;
pub type Script = String;

/// All Jobs in the same stage tend to be run at once.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub stage: Option<StageName>,
    pub before_script: Option<Vec<Script>>,
    pub script: Option<Vec<Script>>,

    /// Even though variables could be None,
    /// they could be defined in extends_job.variables
    /// (or globally)
    pub variables: Option<HashMap<VarName, VarValue>>,

    pub extends: Option<JobName>,

    #[serde(skip)]
    pub extends_job: Option<Rc<Job>>,
}

pub struct GitlabCIConfig {
    /// Based on include orderings, what's the parent of this gitlab config.
    pub parent: Option<Box<GitlabCIConfig>>,

    /// Global variables
    pub variables: HashMap<VarName, VarValue>,

    /// Stages group jobs that run in parallel. The ordering is important
    pub stages: Vec<StageName>,

    /// Targets that gitlab can run.
    pub jobs: HashMap<JobName, Rc<Job>>,
}

fn parse_includes(
    context: &Path,
    include: &Value,
    parent: Option<GitlabCIConfig>,
) -> Option<GitlabCIConfig> {
    match include {
        Value::String(include_filename) => {
            // Remove leading '/' - join (correctly) won't concat them if filename starts from root.
            let include_filename = context.join(&include_filename[1..]);
            parse_aux(&context.join(&Path::new(&include_filename)), parent).ok()
        }
        Value::Sequence(includes) => {
            let mut parent = parent;
            for include in includes {
                parent = parse_includes(context, include, parent);
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
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

///
/// Taking a path to a .gitlab-ci.yml file will read it and parse it where possible.
/// Anything unknown will be silently skipped. Jobs will be linked up with their parents.
///
pub fn parse(gitlab_file: &Path) -> Result<GitlabCIConfig, DynErr> {
    parse_aux(gitlab_file, None)
}

fn parse_aux(gitlab_file: &Path, parent: Option<GitlabCIConfig>) -> Result<GitlabCIConfig, DynErr> {
    //println!("Attempting to open {:?}", gitlab_file);
    let f = std::fs::File::open(&gitlab_file)?;
    let raw_yaml = serde_yaml::from_reader(f)?;

    let val: serde_yaml::Value = merge_keys_serde(raw_yaml).expect("Couldn't merge yaml :<<");
    let mut config = GitlabCIConfig {
        parent: None,
        stages: Vec::new(),
        variables: HashMap::new(),
        jobs: HashMap::new(),
    };

    if let serde_yaml::Value::Mapping(map) = val {
        if let Some(includes) = map.get(&Value::String("include".to_owned())) {
            config.parent = parse_includes(
                gitlab_file
                    .parent()
                    .expect("gitlab-ci file wasn't in a dir??"),
                includes,
                parent,
            )
            .map(Box::new);
        }

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

fn parse_job(config: &GitlabCIConfig, job_name: &str, top: &Mapping) -> Result<Rc<Job>, DynErr> {
    let job_nm = Value::String(job_name.to_owned());
    let j: Result<Job, _> = serde_yaml::from_value(
        top.get(&job_nm)
            .expect(&format!(
                "job asked to be parsed but not found: {}",
                job_name
            ))
            .clone(),
    );
    if let Ok(mut j) = j {
        if let Some(ref parent_job_name) = j.extends {
            // Parse parents first so we don't get wicked fun with Rc<>...

            let job: Option<Rc<Job>> = if job_name != parent_job_name
                && top.contains_key(&Value::String(parent_job_name.clone()))
            {
                parse_job(config, parent_job_name, top).ok()
            } else {
                let mut parent = config.parent.as_ref();
                let mut result = None;
                while parent.is_some() {
                    result = parse_job(
                        parent.expect("gitlab-ci file has no parent dir???"),
                        parent_job_name,
                        top,
                    )
                    .ok();
                    if result.is_some() {
                        break;
                    }
                    parent = parent.expect("can't happen").parent.as_ref();
                }
                result
            };
            j.extends_job = job;
        }
        Ok(Rc::new(j)) //TODO: maybe push rc outside here
    } else {
        Err(Box::new(j.unwrap_err()))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let parent = config
            .jobs
            .get("tired_starlings")
            .unwrap()
            .extends_job
            .as_ref()
            .unwrap();
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

        Ok(())
    }
}
