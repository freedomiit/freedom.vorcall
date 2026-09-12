using System.Net.Http.Headers;

namespace Vorcall.Server.Tests.Infrastructure;

// The attachment endpoints: raw bytes in, raw bytes out, neither of them protobuf.
internal static class Uploads
{
    public const string FileNameHeader = "X-Vorcall-Filename";

    public static Task<ProtoResponse> UploadAsync(
        VorcallFactory factory,
        string bearer,
        string room,
        byte[] body,
        string contentType = "image/png",
        string? fileName = null,
        long? declaredLength = null)
        => Proto.PostBytesAsync(
            factory,
            $"/api/attachments?room={Uri.EscapeDataString(room)}",
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
