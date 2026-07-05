use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    Run,
    Pause,
    Resume,
    Stop,
}

impl ControlCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }
}

pub fn parse_control_command(contents: &str) -> ControlCommand {
    match contents.trim().to_ascii_lowercase().as_str() {
        "pause" => ControlCommand::Pause,
        "resume" => ControlCommand::Resume,
        "stop" => ControlCommand::Stop,
        _ => ControlCommand::Run,
    }
}

pub fn run_dir_for_job(job_id: &str) -> PathBuf {
    PathBuf::from(".bookforge/runs").join(job_id)
}

pub fn control_path_for_job(job_id: &str) -> PathBuf {
    run_dir_for_job(job_id).join("control")
}

pub fn read_control_file(path: &Path) -> io::Result<ControlCommand> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(parse_control_command(&contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ControlCommand::Run),
        Err(err) => Err(err),
    }
}

pub fn write_control_file(path: &Path, command: ControlCommand) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{}\n", command.as_str()))
}

pub fn clear_control_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_known_control_commands() {
        assert_eq!(parse_control_command("pause\n"), ControlCommand::Pause);
        assert_eq!(parse_control_command(" RESUME "), ControlCommand::Resume);
        assert_eq!(parse_control_command("stop"), ControlCommand::Stop);
    }

    #[test]
    fn missing_or_garbage_control_file_means_run() {
        let dir = unique_temp_dir("missing");
        let path = dir.join("control");
        assert_eq!(read_control_file(&path).unwrap(), ControlCommand::Run);

        fs::write(&path, "nonsense").unwrap();
        assert_eq!(read_control_file(&path).unwrap(), ControlCommand::Run);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn writes_and_clears_control_file() {
        let dir = unique_temp_dir("write");
        let path = dir.join("nested").join("control");

        write_control_file(&path, ControlCommand::Pause).unwrap();
        assert_eq!(read_control_file(&path).unwrap(), ControlCommand::Pause);

        clear_control_file(&path).unwrap();
        assert_eq!(read_control_file(&path).unwrap(), ControlCommand::Run);
        let _ = fs::remove_dir_all(dir);
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bookforge-control-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
