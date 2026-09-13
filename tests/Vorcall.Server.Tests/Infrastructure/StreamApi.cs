using System.Net;
using System.Net.Http.Headers;
using Vorcall.Server.Auth;
using Vorcall.Server.Protocol;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests.Infrastructure;

// The four streamed-file routes. A reader's GET does not answer until the owning client has
// pushed or declined, so a test always has both halves in flight at once: the fetch that reads
// its whole body is for a test whose sender half another task is already driving, and OpenAsync
// hands back the live response for a test that wants to read part of it and then leave.
internal static class StreamApi
{
    public const string Octets = "application/octet-stream";

    public static Task<ProtoResponse> OfferAsync(VorcallFactory factory, string bearer, string channel, StreamOffer offer)
        => Proto.PostAsync(factory, $"/api/streams?channel={Uri.EscapeDataString(channel)}", offer, bearer);

    public static Task<ProtoResponse> OfferAsync(VorcallFactory factory, string bearer, long channelId, StreamOffer offer)
        => OfferAsync(factory, bearer, History.Id(channelId), offer);

    // Every range goes on unparsed: half of what the range tests prove is how the endpoint answers
    // a header no client library would have built.
    public static Task<ProtoResponse> FetchAsync(VorcallFactory factory, string bearer, long id, string? range = null)
        => Proto.GetAsync(
            factory,
            $"/api/streams/{id}",
            bearer,
            configure: request =>
            {
                if (range is not null)
                {
                    request.Headers.TryAddWithoutValidation("Range", range);
                }
            });

    public static Task<HttpResponseMessage> OpenAsync(
        VorcallFactory factory,
        string bearer,
        long id,
        string? range = null,
        CancellationToken ct = default)
    {
        var request = new HttpRequestMessage(HttpMethod.Get, $"/api/streams/{id}");
        request.Headers.Add(ServerKeyMiddleware.HeaderName, ServerFixture.ServerKey);
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", bearer);
        if (range is not null)
        {
            request.Headers.TryAddWithoutValidation("Range", range);
        }

        return factory.Client.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, ct);
    }

    public static Task<ProtoResponse> PushAsync(
        VorcallFactory factory,
        string bearer,
        long streamId,
        string transfer,
        byte[] body)
        => Proto.PostBytesAsync(factory, ChunksPath(streamId, transfer), body, Octets, bearer);

    public static Task<ProtoResponse> PushAsync(
        VorcallFactory factory,
        string bearer,
        long streamId,
        string transfer,
        HttpContent content)
        => Proto.SendAsync(factory, HttpMethod.Post, ChunksPath(streamId, transfer), content, bearer, ServerFixture.ServerKey);

    public static Task<ProtoResponse> DeclineAsync(
        VorcallFactory factory,
        string bearer,
        long streamId,
        string transfer,
        string? reason = null)
    {
        var path = $"/api/streams/{streamId}/decline?transfer={Uri.EscapeDataString(transfer)}";
        if (reason is not null)
        {
            path += $"&reason={Uri.EscapeDataString(reason)}";
        }

        return Proto.PostBytesAsync(factory, path, [], Octets, bearer);
    }

    public static string ChunksPath(long streamId, string transfer)
        => $"/api/streams/{streamId}/chunks?transfer={Uri.EscapeDataString(transfer)}";

    // A transfer id as the query string carries it: digits only, which is all the endpoint takes.
    public static string Id(long transferId) => transferId.ToString(System.Globalization.CultureInfo.InvariantCulture);
}

// The owning client's half of a transfer. The server asks for a range down the owner's socket and
// the owner answers over REST, which is the whole shape of this feature: a test drives the reader
// and this at the same time, never one after the other.
internal sealed class Sender(VorcallFactory factory, Account account, byte[] content)
{
    public byte[] Content => content;

    public Account Account => account;

    // The next range the server asked this client for.
    public static async Task<StreamRequest> NextRequestAsync(WsClient socket, TimeSpan? timeout = null)
        => (await socket.ExpectAsync(Kind.StreamRequest, timeout)).StreamRequest;

    public Task<ProtoResponse> PushAsync(StreamRequest request) => PushAsync(request, Slice(request));

    public Task<ProtoResponse> PushAsync(StreamRequest request, byte[] body)
        => StreamApi.PushAsync(factory, account.Access, request.StreamId, StreamApi.Id(request.TransferId), body);

    public Task<ProtoResponse> PushAsync(StreamRequest request, HttpContent body)
        => StreamApi.PushAsync(factory, account.Access, request.StreamId, StreamApi.Id(request.TransferId), body);

    public Task<ProtoResponse> DeclineAsync(StreamRequest request, string? reason = null)
        => StreamApi.DeclineAsync(factory, account.Access, request.StreamId, StreamApi.Id(request.TransferId), reason);

    // Waits to be asked and answers in full: the sender side of an ordinary transfer, which most
    // tests only need to have happened.
    public async Task<StreamRequest> ServeAsync(WsClient socket, TimeSpan? timeout = null)
    {
        var request = await NextRequestAsync(socket, timeout);
        var push = await PushAsync(request);
        Assert.True(
            push.Status == HttpStatusCode.NoContent,
            $"push of {request.Length} bytes at {request.Offset}: {(int)push.Status}");
        return request;
    }

    public byte[] Slice(StreamRequest request)
        => content.AsSpan((int)request.Offset, (int)request.Length).ToArray();
}
