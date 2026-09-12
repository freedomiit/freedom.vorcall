using System.Net;
using System.Net.Http.Headers;
using Google.Protobuf;
using Vorcall.Server.Auth;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Tests.Infrastructure;

// One REST answer: the status, every header (response and content headers alike, joined when
// repeated) and the body bytes, already read so the HttpResponseMessage can go.
internal sealed class ProtoResponse(HttpStatusCode status, IReadOnlyDictionary<string, string> headers, byte[] body)
{
    public HttpStatusCode Status { get; } = status;

    public byte[] Body { get; } = body;

    public string? Header(string name) => headers.TryGetValue(name, out var value) ? value : null;

    // The ApiError every 4xx/429 of this API carries.
    public string Detail => ApiError.Parser.ParseFrom(Body).Detail;

    public T As<T>(MessageParser<T> parser)
        where T : IMessage<T>
        => parser.ParseFrom(Body);
}

// Protobuf over REST with the door key and an optional bearer. Nothing here asserts: the
// caller decides what status it expects.
internal static class Proto
{
    public const string ContentType = "application/x-protobuf";

    public static Task<ProtoResponse> GetAsync(
        VorcallFactory factory,
        string path,
        string? bearer = null,
        string? key = ServerFixture.ServerKey,
        Action<HttpRequestMessage>? configure = null)
        => SendAsync(factory, HttpMethod.Get, path, null, bearer, key, configure);

    public static Task<ProtoResponse> PostAsync(
        VorcallFactory factory,
        string path,
        IMessage body,
        string? bearer = null,
        string? key = ServerFixture.ServerKey)
        => PostBytesAsync(factory, path, body.ToByteArray(), ContentType, bearer, key);

    public static Task<ProtoResponse> PostBytesAsync(
        VorcallFactory factory,
        string path,
        byte[] body,
        string contentType,
        string? bearer = null,
        string? key = ServerFixture.ServerKey,
        Action<HttpRequestMessage>? configure = null)
    {
        var content = new ByteArrayContent(body);
        content.Headers.ContentType = MediaTypeHeaderValue.Parse(contentType);
        return SendAsync(factory, HttpMethod.Post, path, content, bearer, key, configure);
    }

    public static async Task<ProtoResponse> SendAsync(
        VorcallFactory factory,
        HttpMethod method,
        string path,
        HttpContent? content,
        string? bearer,
        string? key,
        Action<HttpRequestMessage>? configure = null)
    {
        using var request = new HttpRequestMessage(method, path) { Content = content };
        if (key is not null)
        {
            request.Headers.Add(ServerKeyMiddleware.HeaderName, key);
        }

        if (bearer is not null)
        {
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", bearer);
        }

        configure?.Invoke(request);

        using var response = await factory.Client.SendAsync(request);
        var body = await response.Content.ReadAsByteArrayAsync();
        var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var (name, values) in response.Headers.Concat(response.Content.Headers))
        {
            headers[name] = string.Join(", ", values);
        }

        return new ProtoResponse(response.StatusCode, headers, body);
    }
}
