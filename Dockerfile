# atx-mcp の OCI イメージ(ghcr.io/gridhra/atx-mcp)。
#
# ここではコンパイルしない。release.yml の build ジョブが作った**静的リンクの musl
# バイナリ**をそのまま入れるだけなので、ビルドコンテキストは以下の形で用意する:
#
#   <context>/amd64/atx-mcp   # x86_64-unknown-linux-musl の成果物
#   <context>/arm64/atx-mcp   # aarch64-unknown-linux-musl の成果物
#   <context>/LICENSE
#
# `TARGETARCH` は buildx が `--platform linux/amd64,linux/arm64` から自動で入れる
# (amd64 / arm64)。ローカルで 1 アーキだけ組むなら明示する:
#   docker build --build-arg TARGETARCH=arm64 -t atx-mcp:local <context>
#
# `FROM scratch` にできるのは、バイナリが libc も CA 証明書も必要としないため
# (外部通信なし、フォントは同梱、stdio だけで話す)。
FROM scratch

ARG TARGETARCH
ARG VERSION=0.0.0

# MCP 公式レジストリ(registry.modelcontextprotocol.io)はこのラベルで
# イメージの所有権を検証する。server.json の `name` と**完全一致**させる。
LABEL io.modelcontextprotocol.server.name="io.github.gridhra/atx-mcp"

LABEL org.opencontainers.image.title="atx-mcp" \
      org.opencontainers.image.description="Deterministic, non-generative image transform MCP server: reproducible recipes, immutable originals." \
      org.opencontainers.image.source="https://github.com/gridhra/atx-mcp" \
      org.opencontainers.image.url="https://github.com/gridhra/atx-mcp" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}"

COPY LICENSE /LICENSE
COPY --chmod=0755 ${TARGETARCH}/atx-mcp /atx-mcp

# アセットストアの置き場。ホスト側を `-v "$PWD:/workspace"` で束ねて使う。
VOLUME ["/workspace"]

# stdio トランスポートなので、必ず標準入力を開いたまま起動する
# (`docker run -i --rm -v "$PWD:/workspace" ghcr.io/gridhra/atx-mcp:<version>`)。
# import_asset へ渡すパスは**コンテナ内のパス**(`/workspace/...`)。
ENTRYPOINT ["/atx-mcp"]
CMD ["--workspace", "/workspace"]
