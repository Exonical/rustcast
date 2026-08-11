use std::io;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: go-junit <output.xml>")?;
    let failed = go_junit::write_report(io::BufReader::new(io::stdin().lock()), path)?;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
