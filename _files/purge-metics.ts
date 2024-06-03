import * as AWS from 'aws-sdk';
const s3 = new AWS.S3();

// Define the bucket name and parameters
const bucketName = 'skippr-metric-prod';
const basePrefix = 'compacted/metric/';
const dates = ['2024-04-10', '2024-04-11', '2024-04-12'];

async function deleteObjects() {
    let continuationToken: string | undefined;

    do {
        // List objects with pagination
        const params: AWS.S3.ListObjectsV2Request = {
            Bucket: bucketName,
            Prefix: basePrefix,
            MaxKeys: 10000,
            ContinuationToken: continuationToken
        };

        try {
            const data = await s3.listObjectsV2(params).promise();
            continuationToken = data.NextContinuationToken;

            // Filter and delete objects that match the date criteria
            const keysToDelete = data.Contents?.filter(key =>
                key.Key && dates.some(date => key.Key!.includes(date))
            ).map(key => ({ Key: key.Key! }));

            if (keysToDelete && keysToDelete.length > 0) {

                console.log('Deleting:', keysToDelete.length, 'objects');

                const deleteParams: AWS.S3.DeleteObjectsRequest = {
                    Bucket: bucketName,
                    Delete: { Objects: keysToDelete }
                };

                const deleteResult = await s3.deleteObjects(deleteParams).promise();
                console.log('Deleted:', deleteResult.Deleted?.length, 'objects');
            }
        } catch (error) {
            console.error("Error in processing:", error);
            break;
        }
    } while (continuationToken);
}

deleteObjects().then(() => console.log('Deletion process completed.'));
