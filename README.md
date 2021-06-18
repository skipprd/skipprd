

 
## Local Build and Testing

1. Docker Login to pull base image

```
 AWS_PROFILE=skippr docker login --username AWS --password $(AWS_PROFILE=skippr aws ecr get-login-password --region eu-west-2) 536671797322.dkr.ecr.eu-west-2.amazonaws.com
```
 
2. export ssh key to authenticate with gitlab for php composer
```
export SSH_PRIVATE_KEY=`cat ~/.ssh/skippr/id_rsa_deployer`
```

3. build with local tag

```
docker build --build-arg SSH_PRIVATE_KEY="${SSH_PRIVATE_KEY}" -f ./Dockerfile -t skipprd:build .
```


## Integration Testing

The integration tests run against a container image. Expect a docker image named 'skippr/skipprd:build'.

Build as above in local build and then run integration tests 
