# Consumed by GoReleaser: it copies the already cross-compiled binary out of the
# build context rather than compiling, so the image build is fast and uses the
# same static binary every other artifact ships.
#
# kura is a pure-Go CLI with no browser and no runtime dependencies beyond CA
# certificates, so the image is tiny: just the static binary on a minimal base.
#
# GoReleaser builds one multi-platform image with buildx and stages each
# platform's binary under a $TARGETPLATFORM directory (e.g. linux/amd64/) in the
# build context, so the COPY line selects the right one through the automatic
# TARGETPLATFORM build arg.
FROM alpine:3.21

ARG TARGETPLATFORM

# ca-certificates for HTTPS to YouTube; tzdata for sane timestamps.
RUN apk add --no-cache ca-certificates tzdata \
 && adduser -D -H -u 10001 kura \
 && mkdir -p /out \
 && chown kura:kura /out

COPY $TARGETPLATFORM/kura /usr/bin/kura

USER kura
WORKDIR /out

# Archives are written under /out by default:
#
#   docker run --rm -v "$PWD/out:/out" ghcr.io/tamnd/kura archive dQw4w9WgXcQ
#
# The kura user has no home directory of its own, so HOME points at the mounted
# /out volume. That keeps kura's default output and resume state writable (it
# lands under $HOME/data/kura).
ENV KURA_OUT=/out \
    HOME=/out

VOLUME ["/out"]

ENTRYPOINT ["/usr/bin/kura"]
