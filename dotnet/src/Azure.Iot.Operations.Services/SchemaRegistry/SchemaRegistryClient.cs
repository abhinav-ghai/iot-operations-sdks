﻿// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

namespace Azure.Iot.Operations.Services.SchemaRegistry;

using Azure.Iot.Operations.Protocol;
using Azure.Iot.Operations.Services.SchemaRegistry.SchemaRegistry;
using SchemaInfo = SchemaRegistry.Schema;
using SchemaFormat = SchemaRegistry.Format;
using SchemaType = SchemaRegistry.SchemaType;
using Azure.Iot.Operations.Protocol.RPC;

public class SchemaRegistryClient(ApplicationContext applicationContext, IMqttPubSubClient pubSubClient) : ISchemaRegistryClient
{
    private static readonly TimeSpan s_DefaultCommandTimeout = TimeSpan.FromSeconds(10);
    private readonly SchemaRegistryClientStub _clientStub = new(applicationContext, pubSubClient);
    private bool _disposed;

    /// <inheritdoc/>
    public async Task<SchemaInfo?> GetAsync(
        string schemaId,
        string version = "1",
        TimeSpan? timeout = null,
        CancellationToken cancellationToken = default)
    {
        try
        {
            cancellationToken.ThrowIfCancellationRequested();
            ObjectDisposedException.ThrowIf(_disposed, this);

            return (await _clientStub.GetAsync(
                new GetRequestPayload()
                {
                    GetSchemaRequest = new()
                    {
                        Name = schemaId,
                        Version = version
                    }
                }, null, null, timeout ?? s_DefaultCommandTimeout, cancellationToken)).Schema;
        }
        catch (AkriMqttException ex) when (ex.Kind == AkriMqttErrorKind.PayloadInvalid)
        {
            // This is likely because the user received a "not found" response payload from the service, but the service is an
            // older version that sends an empty payload instead of the expected "{}" payload.
            return null;
        }
        catch (AkriMqttException e) when (e.Kind == AkriMqttErrorKind.UnknownError)
        {
            // ADR 15 specifies that schema registry clients should still throw a distinct error when the service returns a 422. It also specifies
            // that the protocol layer should no longer recognize 422 as an expected error kind, so assume unknown errors are just 422's
            throw new SchemaRegistryServiceException("Invocation error returned by schema registry service", e.PropertyName, e.PropertyValue);
        }
    }

    /// <inheritdoc/>
    public async Task<SchemaInfo?> PutAsync(
        string schemaContent,
        SchemaFormat schemaFormat,
        SchemaType schemaType = SchemaType.MessageSchema,
        string version = "1",
        Dictionary<string, string>? tags = null,
        TimeSpan? timeout = null,
        CancellationToken cancellationToken = default)
    {
        try
        { 
            cancellationToken.ThrowIfCancellationRequested();
            ObjectDisposedException.ThrowIf(_disposed, this);

            return (await _clientStub.PutAsync(
                new PutRequestPayload()
                {
                    PutSchemaRequest = new()
                    {
                        Format = schemaFormat,
                        SchemaContent = schemaContent,
                        Version = version,
                        Tags = tags,
                        SchemaType = schemaType
                    }
                }, null, null, timeout ?? s_DefaultCommandTimeout, cancellationToken)).Schema;
        }
        catch (AkriMqttException e) when (e.Kind == AkriMqttErrorKind.UnknownError)
        {
            // ADR 15 specifies that schema registry clients should still throw a distinct error when the service returns a 422. It also specifies
            // that the protocol layer should no longer recognize 422 as an expected error kind, so assume unknown errors are just 422's
            throw new SchemaRegistryServiceException("Invocation error returned by schema registry service", e.PropertyName, e.PropertyValue);
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (_disposed)
        {
            return;
        }

        await _clientStub.DisposeAsync().ConfigureAwait(false);
        GC.SuppressFinalize(this);
        _disposed = true;
    }
}
