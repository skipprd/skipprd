
## Build

export SSH_PRIVATE_KEY=`cat ~/.ssh/skippr/id_rsa_deployer`
 
docker build --build-arg SSH_PRIVATE_KEY="${SSH_PRIVATE_KEY}" -f ./Dockerfile -t skippr/skipprd:latest .

 
## Local Testing

1. Docker Login to pull base image

```
 AWS_PROFILE=skippr docker login --username AWS --password $(AWS_PROFILE=skippr aws ecr get-login-password --region eu-west-2) 536671797322.dkr.ecr.eu-west-2.amazonaws.com
```
 
2. build with local tag

```
docker build --build-arg SSH_PRIVATE_KEY="${SSH_PRIVATE_KEY}" -f ./Dockerfile -t skipprd:build .
```

