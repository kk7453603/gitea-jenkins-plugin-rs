/*
 See the documentation for more options:
 https://github.com/jenkins-infra/pipeline-library/
*/

// Build and test the Rust native library on a Linux/amd64 agent before the
// Maven-driven plugin build. The `mvn package` step invoked by `buildPlugin`
// re-runs `cargo build --release` on the generate-resources phase (see pom.xml)
// and bundles libgitea_rust.so into the .hpi under
// META-INF/native/linux/amd64/.
stage('Build Rust') {
  node('linux && amd64') {
    checkout scm
    sh 'cargo --version'
    dir('rust/gitea-client') {
      sh 'cargo build --release'
      sh 'cargo test'
    }
    stash name: 'native-lib', includes: 'rust/gitea-client/target/release/libgitea_rust.so'
  }
}

buildPlugin(
  useContainerAgent: true,
  configurations: [
    [platform: 'linux', jdk: 21],
    [platform: 'windows', jdk: 17],
 ],
)
