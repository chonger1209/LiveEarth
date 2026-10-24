// Translate rustc's x64 dlltool invocation to the LLVM librarian bundled with Rust.
// This avoids GNU dlltool's external assembler dependency and ANSI path handling.
use std::{env, ffi::OsString, process::{Command, ExitCode}};
fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let value = |name: &str| args.iter().position(|v| v == name).and_then(|i| args.get(i+1));
    let (Some(def),Some(output),Some(name))=(value("-d"),value("-l"),value("-D")) else {eprintln!("dlltool bridge: missing -d/-l/-D");return ExitCode::FAILURE};
    if value("-m").is_some_and(|machine| machine != "i386:x86-64") { eprintln!("dlltool bridge only supports x86_64");return ExitCode::FAILURE; }
    let argument = |prefix: &str, value: &OsString| { let mut out=OsString::from(prefix);out.push(value);out };
    let Some(lld)=env::var_os("EARTHLIVE_LLD") else {eprintln!("EARTHLIVE_LLD missing");return ExitCode::FAILURE};
    let generated = std::path::PathBuf::from(def).with_extension("llvm.def");
    let result = std::fs::read_to_string(def).and_then(|body| std::fs::write(&generated, format!("LIBRARY \"{}\"\n{body}", name.to_string_lossy())));
    if let Err(error) = result { eprintln!("import definition: {error}"); return ExitCode::FAILURE; }
    let status = Command::new(lld).args([OsString::from("-flavor"),OsString::from("link"),OsString::from("/lib"),OsString::from("/machine:x64"),argument("/def:",&generated.into_os_string()),argument("/out:",output)]).status();
    match status {
        Ok(status) if status.success()=>ExitCode::SUCCESS,
        Ok(status)=>{eprintln!("LLVM librarian failed: {status}");ExitCode::FAILURE},
        Err(error)=>{eprintln!("LLVM librarian: {error}");ExitCode::FAILURE},
    }
}
