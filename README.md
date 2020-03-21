# docker-rust-test

This is a simple test repository. It used to conatin a hello-world-application written in Rust, and some docker fluff:

 * compile inside a docker container
 * copy binary into another container

We currently use it to test rust development in general, and to check if this works with cros-scompiling.

## How to cross-compile

Use `docker buildx build --platform linux/amd64,linux/arm/v7 -t dystonse/rust-test:latest --push .` to build and push the containers for both `linux/amd64` and `linux/arm/v7` architectures.

You might have to enable experimental features first, e.g. using `export DOCKER_CLI_EXPERIMENTAL=enabled`.

Also, you might have to create and activate a builder, as documented [here for Docker Desktop (Mac and Windows)](https://docs.docker.com/docker-for-mac/multi-arch/) or [here for Linux hosts](https://mirailabs.io/blog/multiarch-docker-with-buildx/).

## Known problems with cross-compiling
We hit a problem when cross-compiling a rust application with dependencies on Docker Desktop for Mac. While building the arm/v7 container, `cargo build` can't read some git-specific directory, as explained in [this issue](https://github.com/rust-lang/cargo/issues/7451).

It boils down to a broken emulation of system calls when emulating a 32-Bit system on a 64-bit host using qemu. The actual bug - if you call it a bug - is not in `qemu` but in `libc`.

A good workaround should be to use a host kernel which has been compiled with the `CONFIG_X86_X32` configuration flag. Docker Desktop for Mac used a virtualized Linux host using HyperKit. The linux image is build with `LinuxKit`, however, we could not verify if the image shipped with Docker Desktop has the `CONFIG_X86_X32` configuration flag (probably not).

But the same error occurs when cross-compiling on another host, which runs a Debian Linux natively. According to its `/boot/config-4.19.0-8-amd64` file, the `CONFIG_X86_X32` configuration is enabled there, so it should have worked.