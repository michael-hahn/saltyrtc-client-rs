# syntax=docker/dockerfile:1
FROM rust:1.51

WORKDIR /saltyrtc-client-rs

COPY ./ ./

# We use our custom OpenSSL configuration file (with lower security level)
# Reference: 
# https://askubuntu.com/questions/1233186/ubuntu-20-04-how-to-set-lower-ssl-security-level
# https://superuser.com/questions/1640089/ssl-certificate-ee-certificate-key-too-weak
ENV OPENSSL_CONF=./openssl.cnf

RUN cargo build --example chat

ENTRYPOINT ["./target/debug/examples/chat"]
# Default command is 'initiator', you can override to responder in `docker run`
CMD ["initiator"]
