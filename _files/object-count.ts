import * as AWS from 'aws-sdk';
import * as fs from 'fs';
import * as path from 'path';

// Initialize S3 client
const s3 = new AWS.S3();

// Replace these with your actual bucket name and root prefix
const bucket = 'skippr-dev-datalake';
let rootPrefix = 'small_files_7/';

// Ensure rootPrefix ends with a slash
if (!rootPrefix.endsWith('/')) {
    rootPrefix += '/';
}

const tempFilePath = path.join(__dirname, 'temp_results.txt');
fs.writeFileSync(tempFilePath, '', 'utf8');

async function countObjects(currentPrefix: string): Promise<void> {
    console.log(`Processing ${currentPrefix}`);
    const params = {
        Bucket: bucket,
        Prefix: currentPrefix,
    };

    try {
        // Count objects for the current prefix
        const objects = await s3.listObjects(params).promise();
        const count = objects.Contents ? objects.Contents.length : 0;
        fs.appendFileSync(tempFilePath, `${currentPrefix} ${count}\n`);

        // Find sub-prefixes and recurse
        const delimiterParams = { ...params, Delimiter: '/' };
        const subPrefixes = await s3.listObjects(delimiterParams).promise();

        if (subPrefixes.CommonPrefixes) {
            for (const subPrefix of subPrefixes.CommonPrefixes) {
                if (subPrefix.Prefix) {
                    await countObjects(subPrefix.Prefix);
                }
            }
        }
    } catch (error) {
        console.error(`Error processing ${currentPrefix}: `, error);
    }
}

async function main() {
    await countObjects(rootPrefix);

    // Read, sort, and log the results
    const results = fs.readFileSync(tempFilePath, 'utf8')
        .split('\n')
        .filter(line => line !== '')
        .map(line => ({ prefix: line.split(' ')[0], count: parseInt(line.split(' ')[1], 10) }))
        .sort((a, b) => b.count - a.count);

    console.log('Results (sorted by count):');
    results.forEach(result => console.log(`${result.prefix} ${result.count}`));

    // Clean up
    fs.unlinkSync(tempFilePath);
}

main().catch(console.error);
