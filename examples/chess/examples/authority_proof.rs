#[path = "../proof/authority_proof.rs"]
mod authority_proof;

use authority_proof::{render_process_result, run_authority_proof, ProofOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let output = render_process_result(run_authority_proof(ProofOptions::default()).await);
    print!("{}", output.stdout);
    eprint!("{}", output.stderr);
    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }
}
