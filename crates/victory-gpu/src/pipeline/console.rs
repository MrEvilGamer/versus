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
    Some(compute(desc, iid, out))
}
