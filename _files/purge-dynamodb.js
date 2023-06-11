const AWS = require('aws-sdk');

AWS.config.update({
    region: 'us-east-1',
    accessKeyId: process.env.AWS_ACCESS_KEY_ID,
    secretAccessKey: process.env.AWS_SECRET_ACCESS_KEY,
});

const docClient = new AWS.DynamoDB.DocumentClient();

const params = {
    TableName: 'Test-MetadataService-Stack-MetadataTable8CB34826-1OBKKG0QKVLJC',
};

console.log('Scanning DynamoDB table.');

docClient.scan(params, function onScan(err, data) {
    if (err) {
        console.error('Unable to scan the table:', JSON.stringify(err, null, 2));
    } else {
        console.log('Scan succeeded.');

        data.Items.forEach((item) => {
            console.log('Deleting item:', item);

            const deleteParams = {
                TableName: params.TableName,
                Key: {
                    tenant: item.tenant,
                    time: item.time
                },
            };

            docClient.delete(deleteParams, function(err, data) {
                if (err) console.error('Unable to delete item:', JSON.stringify(err, null, 2));
                else console.log('Delete succeeded.');
            });
        });

        // Continue scanning if we have more items
        if (typeof data.LastEvaluatedKey !== 'undefined') {
            params.ExclusiveStartKey = data.LastEvaluatedKey;
            docClient.scan(params, onScan);
        }
    }
});
