using System.Net.Http.Headers;

namespace Vorcall.Server.Tests.Infrastructure;

// The attachment endpoints: raw bytes in, raw bytes out, neither of them protobuf. The channel is
// a string here for the same reason it is in History: what the endpoint does with a value that is
// not a channel id is part of what its tests prove.
internal static class Uploads
{
    public const string FileNameHeader = "X-Vorcall-Filename";

    public static Task<ProtoResponse> UploadAsync(
        VorcallFactory factory,
        string bearer,
        string channel,
        byte[] body,
        string contentType = "image/png",
        string? fileName = null,
        long? declaredLength = null)
        => Proto.PostBytesAsync(
            factory,
            $"/api/attachments?channel={Uri.EscapeDataString(channel)}",
            body,
            contentType,
            bearer,
            configure: request =>
            {
                if (fileName is not null)
                {
                    request.Headers.Add(FileNameHeader, fileName);
                }

                // A length the body does not have: what the endpoint refuses from the header
                // alone, before reading a byte.
                if (declaredLength is { } length)
                {
                    request.Content!.Headers.ContentLength = length;
                }
            });

    public static Task<ProtoResponse> UploadAsync(
        VorcallFactory factory,
        string bearer,
        long channelId,
        byte[] body,
        string contentType = "image/png",
        string? fileName = null,
        long? declaredLength = null)
        => UploadAsync(factory, bearer, History.Id(channelId), body, contentType, fileName, declaredLength);

    public static Task<ProtoResponse> DownloadAsync(VorcallFactory factory, string bearer, long id, string? range = null)
        => Proto.GetAsync(
            factory,
            $"/api/attachments/{id}",
            bearer,
            configure: request =>
            {
                if (range is not null)
                {
                    request.Headers.Range = RangeHeaderValue.Parse(range);
                }
            });
}
