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
        string? contentType = "image/png",
        string? fileName = null,
        long? declaredLength = null,
        string? rawContentType = null)
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

                // A Content-Type no client library would build: it goes on unparsed, because
                // what the endpoint does with a header that is not a media type is the point.
                if (rawContentType is not null)
                {
                    request.Content!.Headers.Remove("Content-Type");
                    request.Content.Headers.TryAddWithoutValidation("Content-Type", rawContentType);
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
        string? contentType = "image/png",
        string? fileName = null,
        long? declaredLength = null,
        string? rawContentType = null)
        => UploadAsync(factory, bearer, History.Id(channelId), body, contentType, fileName, declaredLength, rawContentType);

    // POST /api/images?purpose=..., the image store's own upload: four types, magic-checked,
    // 8 MiB. Same door key, bearer and rate-limit policy as an attachment's.
    public static Task<ProtoResponse> UploadImageAsync(
        VorcallFactory factory,
        string bearer,
        string purpose,
        byte[] body,
        string? contentType = "image/png",
        long? declaredLength = null)
        => Proto.PostBytesAsync(
            factory,
            $"/api/images?purpose={Uri.EscapeDataString(purpose)}",
            body,
            contentType,
            bearer,
            configure: request =>
            {
                if (declaredLength is { } length)
                {
                    request.Content!.Headers.ContentLength = length;
                }
            });

    // POST /api/sounds?name=..., the soundpad store's own upload: a VORCSND1 container at
    // 16 MiB, behind MANAGE_SOUNDS. Same door key, bearer and rate-limit policy as an attachment's.
    public static Task<ProtoResponse> UploadSoundAsync(
        VorcallFactory factory,
        string bearer,
        string name,
        byte[] body,
        string? contentType = Vorcall.Server.Attachments.AttachmentsOptions.SoundMediaType,
        long? declaredLength = null)
        => Proto.PostBytesAsync(
            factory,
            $"/api/sounds?name={Uri.EscapeDataString(name)}",
            body,
            contentType,
            bearer,
            configure: request =>
            {
                if (declaredLength is { } length)
                {
                    request.Content!.Headers.ContentLength = length;
                }
            });

    // POST /api/stickers?name=..., the sticker store's own upload: four image types, magic-checked,
    // at 1 MiB, behind MANAGE_STICKERS. Same door key, bearer and rate-limit policy as an attachment's.
    public static Task<ProtoResponse> UploadStickerAsync(
        VorcallFactory factory,
        string bearer,
        string name,
        byte[] body,
        string? contentType = "image/png",
        long? declaredLength = null)
        => Proto.PostBytesAsync(
            factory,
            $"/api/stickers?name={Uri.EscapeDataString(name)}",
            body,
            contentType,
            bearer,
            configure: request =>
            {
                if (declaredLength is { } length)
                {
                    request.Content!.Headers.ContentLength = length;
                }
            });

    public static Task<ProtoResponse> DownloadStickerAsync(VorcallFactory factory, string bearer, long id)
        => Proto.GetAsync(factory, $"/api/stickers/{id}", bearer);

    public static Task<ProtoResponse> DownloadSoundAsync(VorcallFactory factory, string bearer, long id, string? range = null)
        => Proto.GetAsync(
            factory,
            $"/api/sounds/{id}",
            bearer,
            configure: request =>
            {
                if (range is not null)
                {
                    request.Headers.Range = RangeHeaderValue.Parse(range);
                }
            });

    public static Task<ProtoResponse> DownloadImageAsync(VorcallFactory factory, string bearer, long id, string? range = null)
        => Proto.GetAsync(
            factory,
            $"/api/images/{id}",
            bearer,
            configure: request =>
            {
                if (range is not null)
                {
                    request.Headers.Range = RangeHeaderValue.Parse(range);
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
