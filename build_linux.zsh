#!/bin/zsh
IMAGE_TAG=`whoami`-test
DOCKER_BUILDKIT=0 docker buildx build --ulimit nofile=1024000:1024000 --platform linux/amd64 -t statsig/statsig-forward-proxy:${IMAGE_TAG} -f Dockerfile --load .
docker push statsig/statsig-forward-proxy:${IMAGE_TAG}
