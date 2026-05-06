use anyhow::Result;

use crate::checkpoint::{CheckpointSender, CheckpointWriter};

pub async fn finalize_writer<T>(
    translation_result: Result<T, anyhow::Error>,
    sender: CheckpointSender,
    writer: CheckpointWriter,
) -> Result<T> {
    drop(sender);
    let writer_result = writer.shutdown().await;
    match (translation_result, writer_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(writer_err)) => Err(writer_err),
        (Err(translation_err), Ok(())) => Err(translation_err),
        (Err(translation_err), Err(writer_err)) => Err(anyhow::anyhow!(
            "{translation_err}; additionally checkpoint writer failed: {writer_err}"
        )),
    }
}
