#[macro_use]
extern crate serde_derive;
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
    pub variables: Option<HashMap<VarName, VarValue>>,
    pub extends: Option<JobName>,

    #[serde(skip)]
    pub extends_job: Option<Rc<Job>>,
}

pub struct GitlabCIConfig {
    /// Resolved inclusions preserving order.
    pub include: Vec<GitlabCIConfig>,

    /// Global variables
    pub variables: HashMap<VarName, VarValue>,

    /// Stages group jobs that run in parallel. The ordering is important
    pub stages: Vec<StageName>,

    /// Targets that gitlab can run.
    pub jobs: HashMap<JobName, Rc<Job>>,
}

fn parse_includes(include: &Value) -> Vec<GitlabCIConfig> {
    let mut results = vec![];
    match include {
        Value::String(include_filename) => {
            if let Ok(result) = parse(&Path::new(&include_filename)) {
                results.push(result);
            }
        }
        Value::Sequence(includes) => {
            for include in includes {
                parse_includes(include);
            }
        }
        Value::Mapping(map) => {
            if let Some(Value::String(local)) = map.get(&Value::String("local".to_owned())) {
                if let Ok(result) = parse(&Path::new(local)) {
                    results.push(result);
                }
            } else if let Some(Value::String(project)) =
                map.get(&Value::String("project".to_owned()))
            {
                // We assume that the included project is checked out in a sister directory.
                let parts = project.split('/');
                let project_name = parts.into_iter().last().unwrap();

                if let Value::String(file) = map
                    .get(&Value::String("file".to_owned()))
                    .unwrap_or(&Value::String(".gitlab-ci.yml".to_owned()))
                {
                    let path = Path::new("..")
                        .join(Path::new(project_name))
                        .join(Path::new(file));
                    if let Ok(result) = parse(&path) {
                        results.push(result);
                    }
                }
            }
        }
        _ => {}
    }
    results
}

///
/// Taking a path to a .gitlab-ci.yml file will read it and parse it where possible.
/// Anything unknown will be silently skipped. Jobs will be linked up with their parents.
///
pub fn parse(gitlab_file: &Path) -> Result<GitlabCIConfig, DynErr> {
    let f = std::fs::File::open(&gitlab_file)?;
    let raw_yaml = serde_yaml::from_reader(f)?;

    let val: serde_yaml::Value = merge_keys_serde(raw_yaml).unwrap();
    let mut config = GitlabCIConfig {
        include: Vec::new(),
        stages: Vec::new(),
        variables: HashMap::new(),
        jobs: HashMap::new(),
    };

    if let serde_yaml::Value::Mapping(map) = val {
        if let Some(includes) = map.get(&Value::String("include".to_owned())) {
            config.include = parse_includes(includes);
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
                            parse_job(&mut config, k, &map);
                        }
                    };
                }
            }
        }
    }

    Ok(config)
}

fn parse_job(mut config: &mut GitlabCIConfig, job_name: &str, top: &Mapping) {
    let job_nm = Value::String(job_name.to_owned());
    let j: Result<Job, _> = serde_yaml::from_value(top.get(&job_nm).unwrap().clone());
    if let Ok(mut j) = j {
        if let Some(ref parent_job_name) = j.extends {
            // Parse parents first so we don't get wicked fun with Rc<>...
            parse_job(&mut config, parent_job_name, top);
            j.extends_job = Some(config.jobs.get(parent_job_name).unwrap().clone());
        }
        config.jobs.insert(job_name.to_owned(), Rc::new(j));
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    pub fn parse_example() -> Result<(), DynErr> {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
        let p = &PathBuf::from(Path::join(&root, "examples/simple/.gitlab-ci.yml"));
        let config = parse(p)?;
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
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
        let p = &PathBuf::from(Path::join(&root, ".gitlab-ci.yml"));
        let config = parse(p)?;
        assert_eq!(config.include.len(), 1);

        Ok(())
    }
}
