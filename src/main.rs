// TEMPORARY SPIKE — discovers what the OS reports as an executable path.
// Replaced by the real implementation in the TDD cycles that follow.
use libproc::processes::{pids_by_type, ProcFilter};
use libproc::proc_pid;

fn main() {
    let pids = pids_by_type(ProcFilter::All).expect("listpids");
    println!("total pids visible: {}", pids.len());
    let mut interesting = 0;
    for pid in pids {
        let p = pid as i32;
        match proc_pid::pidpath(p) {
            Ok(path) => {
                let l = path.to_lowercase();
                if l.contains("claude") || l.contains("codex") || l.contains("cursor") || l.contains("gemini") {
                    interesting += 1;
                    println!("  pid={p:<7} {path}");
                }
            }
            Err(_) => {}
        }
    }
    println!("agent-ish exe paths: {interesting}");
}
