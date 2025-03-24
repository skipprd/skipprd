# Configuration: SKIPPR_API_TOKEN

## Description

This configuration is used to sync metadata to Skippr Metadata API and validate a paid subscription license.

## Default Value

No default value. The SKIPPR_API_TOKEN must be provided by the user.

## Example Values

Let's consider an example where the user has a Skippr API token as "abc123".

- SKIPPR_API_TOKEN=abc123

In this example, the `abc123` token will be used to authenticate requests to the Skippr Metadata API and verify the user's license.

## Detailed Description

The SKIPPR_API_TOKEN is a unique identifier associated with each user account. The API uses this token to authenticate requests from users and ensure they have valid subscriptions. This configuration is necessary for syncing metadata to the Skippr Metadata API.

## Considerations

When setting up Skippr, special attention should be given to the SKIPPR_API_TOKEN configuration. Here are a few considerations:

- The SKIPPR_API_TOKEN is unique to each user account. Do not share your token with others, as it provides access to your account and data.

- Keep your API token secure. If your token is lost or compromised, you should revoke it immediately and generate a new one.

- The API token is necessary for validating the subscription license. Without a valid license, Skippr will not function as expected.

- Always ensure that the SKIPPR_API_TOKEN is correctly configured in your environment. Incorrect or missing configuration may result in failure to sync metadata or validate the license.

