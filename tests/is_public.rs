use gitlab_ci_parser::*;

#[test]
pub fn test() {
    let _j = Job {
        before_script: None,
        script: None,
        extends: None,
        extends_job: None,
        stage: None,
        variables: None,
    };
}
