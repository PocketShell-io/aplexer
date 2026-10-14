//! Bounded PSC-RPC-1 frame codec and response encoding.
use super::*;

pub(super) fn read_packet(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut header = [0; 4];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[1..])
        .context("truncated RPC frame header")?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > FRAME_LIMIT {
        bail!("RPC frame length outside 1..={FRAME_LIMIT}");
    }
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .context("truncated RPC frame payload")?;
    Ok(Some(bytes))
}
pub(super) fn write_packet(writer: &mut impl Write, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > FRAME_LIMIT {
        bytes = serde_json::to_vec(&json!({"protocol":1,"id":value["id"],"error":{
            "code":"response_too_large","message":"response exceeds frame limit","notDispatched":false}}))?;
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}
pub(super) fn write_cached_result(
    writer: &mut impl Write,
    id: &Value,
    data: &[u8],
    meta: &Value,
) -> Result<()> {
    let prefix = format!(
        "{{\"protocol\":1,\"id\":{},\"result\":",
        serde_json::to_string(id)?
    );
    let suffix = format!(",\"meta\":{}}}", serde_json::to_string(meta)?);
    let length = prefix.len() + data.len() + suffix.len();
    if length > FRAME_LIMIT {
        return write_packet(
            writer,
            &failure(
                id.clone(),
                "response_too_large",
                "response exceeds frame limit",
                true,
            ),
        );
    }
    writer.write_all(&(length as u32).to_be_bytes())?;
    writer.write_all(prefix.as_bytes())?;
    writer.write_all(data)?;
    writer.write_all(suffix.as_bytes())?;
    writer.flush()?;
    Ok(())
}
pub(super) fn failure(
    id: Value,
    code: &str,
    message: impl ToString,
    not_dispatched: bool,
) -> Value {
    json!({"protocol":1,"id":id,"error":{"code":code,"message":message.to_string(),"notDispatched":not_dispatched}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_bounds_and_truncation() {
        assert!(read_packet(&mut &0u32.to_be_bytes()[..]).is_err());
        assert!(read_packet(&mut &((FRAME_LIMIT + 1) as u32).to_be_bytes()[..]).is_err());
        assert!(read_packet(&mut &[0, 0, 0, 2, b'{'][..]).is_err());
        assert!(read_packet(&mut &[][..]).unwrap().is_none());
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &json!({"id":"a"})).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&read_packet(&mut &bytes[..]).unwrap().unwrap())
                .unwrap(),
            json!({"id":"a"})
        );
    }
}
