<?php

namespace legacy\src\Skipprd;

enum Metrics
{
    const TOTAL = 'Total';
    const GLOBBING = 'Globbing files';
    const WAITING_FOR_FILES = 'Waiting for files';
    const INGESTING = 'Ingesting';
    const CREATE_S3_CLIENT = 'Create S3 client';
    const ASYNC_GETS = 'Async gets';
    const ASYNC_GET_OBJECT = 'Async get s3 object';
    const AWAIT = 'Await thread';
}