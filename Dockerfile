FROM alpine:3.10
RUN apk add --update docker openrc
RUN rc-update add docker boot
