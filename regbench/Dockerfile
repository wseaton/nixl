ARG FEDORA_VERSION=42
FROM fedora:${FEDORA_VERSION}

RUN dnf install -y --setopt=install_weak_deps=False \
        gcc gcc-c++ make cmake meson ninja-build pkgconf-pkg-config git-core \
        autoconf automake libtool rdma-core rdma-core-devel libibverbs-utils \
        numactl-devel hwloc-devel libaio-devel liburing-devel libstdc++-devel \
        clang clang-devel llvm-devel python3 python3-devel pybind11-devel \
        curl ca-certificates findutils which xz procps-ng \
    && dnf clean all

ARG RUST_VERSION=1.100.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustc --version

ARG UCX_REF=v1.23.x
RUN git clone --depth 1 -b ${UCX_REF} https://github.com/openucx/ucx.git /src/ucx \
    && cd /src/ucx && ./autogen.sh \
    && ./contrib/configure-release-mt --prefix=/usr/local/ucx \
        --enable-shared --disable-static --disable-doxygen-doc \
        --enable-optimizations --without-avx --enable-cma \
        --without-cuda --without-rocm --with-verbs --with-rdmacm \
    && make -j"$(nproc)" && make install \
    && rm -rf /src/ucx

ENV PKG_CONFIG_PATH="/usr/local/lib64/pkgconfig:/usr/local/lib/pkgconfig:/usr/local/ucx/lib/pkgconfig:/usr/lib64/pkgconfig:/usr/share/pkgconfig"
ENV NIXL_PREFIX=/usr/local

COPY . /src/nixl
RUN cd /src/nixl \
    && meson setup build --prefix=/usr/local --buildtype=release \
        -Denable_plugins=UCX \
        -Ddisable_gds_backend=true \
        -Dbuild_tests=false \
        -Dbuild_examples=false \
        -Drust=false \
        -Ducx_path=/usr/local/ucx \
    && ninja -C build install \
    && printf '%s\n' /usr/local/ucx/lib /usr/local/lib64 /usr/local/lib > /etc/ld.so.conf.d/nixl.conf \
    && ldconfig

RUN cd /src/nixl/regbench \
    && cargo build --release \
    && cp target/release/regbench /usr/local/bin/regbench \
    && ldd /usr/local/bin/regbench | grep -E 'nixl|ucp'

ENV PATH="/usr/local/ucx/bin:${PATH}"
ENV HOME=/tmp
ENTRYPOINT ["/usr/local/bin/regbench"]
