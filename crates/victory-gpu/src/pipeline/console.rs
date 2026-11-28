//! Selective native-console pipeline resolution, based on the public layout
//! documented by Mandy/ScarlettPort.  Malformed or unavailable inputs remain
//! E_NOTIMPL and therefore use the title's ordinary fallback.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use super::Shader;
use crate::object::{HRESULT, Iid, This};
use victory_shader::{Container, FourCC, HwStage};

type Refusal = &'static str;
const ADDR: std::ops::Range<u64> = 0x1_0000..0x8000_0000_0000;
static BY_DIGEST: Mutex<Option<HashMap<[u8;16], Arc<Shader>>>> = Mutex::new(None);
static REFUSED: OnceLock<bool> = OnceLock::new();
static PRELOADED: OnceLock<()> = OnceLock::new();

pub(crate) fn on() -> bool {
    *REFUSED.get_or_init(|| std::env::var("VICTORY_PIPELINES_X").ok().is_some_and(|v| v.trim()=="1"))
}

pub(crate) fn container_digest(bytes: &[u8]) -> Option<[u8;16]> {
    let c = Container::parse(bytes).ok()?;
    c.part(FourCC::HASH)?.data.get(4..20)?.try_into().ok()
}

pub(crate) fn file(shader: &Arc<Shader>) {
    if !on() { return; }
    if let Some(d) = shader.digest {
        BY_DIGEST.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry(d).or_insert_with(|| shader.clone());
    }
}

/// Index legally supplied, offline-carved shader containers before the title
/// starts asking for deserialized pipelines.  Runtime 0x218 calls commonly
/// precede the ordinary 0x1f0 shader creations, so encounter-only indexing
/// guarantees a miss even when the matching container is available locally.
pub(crate) fn preload() {
    // Keep the stable digest-by-encounter path as the default.  The offline
    // corpus is opt-in while its containers are validated against the exact
    // title build; this prevents a stale or incompatible corpus from making
    // startup itself unsafe.
    if !on() || std::env::var("VICTORY_PIPELINE_PRELOAD").ok().as_deref() != Some("1") { return; }
    PRELOADED.get_or_init(|| {
        let Some(root) = std::env::var_os("VICTORY_PIPELINE_CONTAINER_DIR") else { return; };
        let mut count = 0u64;
        let Ok(entries) = std::fs::read_dir(root) else { return; };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("dxbc") { continue; }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue; };
            let stage = if name.contains("_CS_") { "compute" }
                else if name.contains("_VS_") { "vertex" }
                else if name.contains("_PS_") { "pixel" }
                else { continue };
            let Ok(bytes) = std::fs::read(&path) else { continue; };
            let shader = super::shader(stage, &bytes);
            file(&shader);
            count += 1;
        }
        tracing::info!(count, "preloaded offline shader containers for console pipeline identity");
    });
}

fn read(addr: u64, size: u64) -> Result<Vec<u8>, Refusal> {
    if size == 0 || size > (16 << 20) || !ADDR.contains(&addr) || addr.checked_add(size).is_none() { return Err("invalid console pipeline block"); }
    let mut out = vec![0; size as usize];
    if crate::host::read_into(addr as *const u8, &mut out) { Ok(out) } else { Err("unreadable console pipeline block") }
}
fn u64at(b: &[u8], at: usize) -> Option<u64> { Some(u64::from_le_bytes(b.get(at..at+8)?.try_into().ok()?)) }
fn digest(records: &[u8]) -> Result<[u8;16], Refusal> {
    if records.len() < 20 { return Err("short console records"); }
    records.windows(20).rev().find(|x| u32::from_le_bytes(x[..4].try_into().unwrap())==0x43).map(|x| x[4..20].try_into().unwrap()).ok_or("console records have no digest")
}

fn compute(desc: *const u8, iid: *const Iid, out: *mut This) -> Result<HRESULT, Refusal> {
    if !on() { return Err("VICTORY_PIPELINES_X is off"); }
    let d = read(desc as u64, 0x48)?;
    let state = (u64at(&d, 8).ok_or("missing dispatch state")?, u64at(&d,16).unwrap_or(0));
    if state.0 == 0 || state.1 != 28 { return Err("invalid dispatch state"); }
    let rec_addr = u64at(&d, 0x38).ok_or("missing compute records")?;
    let rec_len = u64at(&d, 0x40).ok_or("missing compute record size")?;
    let key = digest(&read(rec_addr, rec_len)?)?;
    let shader = BY_DIGEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m|m.get(&key)).cloned().ok_or("compute container not seen")?;
    let c = Container::parse(&shader.container).map_err(|_|"container parse failed")?;
    let part = c.part(FourCC::DXIL).ok_or("container has no DXIL")?;
    let p = victory_shader::native_programs(part.data).map_err(|_|"program parse failed")?.into_iter().find(|p|p.stage==HwStage::Cs).ok_or("container has no compute program")?;
    let _ = p;
    if shader.translated().is_none() { return Err("compute container does not translate"); }
    let hr = super::hand_out_slot_verified(vec![shader], None, 0x218, iid, out);
    if hr == crate::object::E_NOTIMPL { Err("host compute PSO creation failed") } else { Ok(hr) }
}

pub(crate) fn try_compute(desc: *const u8, iid: *const Iid, out: *mut This) -> Option<Result<HRESULT, Refusal>> {
    preload();
    Some(compute(desc, iid, out))
}

#[cfg(test)]
mod tests {
    use super::digest;
    fn rec(tag: u32, d: [u8; 16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&24u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0x40u32.to_le_bytes());
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&d);
        b
    }
    #[test]
    fn records_digest_is_terminal_and_bounded() {
        let d = [0xabu8; 16];
        assert_eq!(digest(&rec(0x43, d)), Ok(d));
        assert!(digest(&rec(0x44, d)).is_err());
        assert!(digest(&[0u8; 8]).is_err());
    }
}
