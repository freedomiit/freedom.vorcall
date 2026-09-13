using System.Buffers;
using System.Globalization;
using System.Text;
using Microsoft.AspNetCore.Http.Features;
using Microsoft.Net.Http.Headers;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Streams;

namespace Vorcall.Server.Api;

// Under /api so the pre-shared key middleware covers them, and behind a bearer on top. A streamed
// file's bytes never touch this server's disk, and never its memory beyond one copy buffer: a
// reader's GET and the owner's chunk POST meet in StreamRegistry, and the bytes go from the one
// request's body into the other's response as they arrive. Every write to the reader's socket is
// awaited before the owner's is read again, so a slow reader slows the owner and nothing piles up
// in between.
public static class StreamsEndpoints
{
    private const string ChannelField = "channel";
    private const string TransferField = "transfer";
    private const string SizeField = "size";

    private const string NotFoundDetail = "no such streamed file";
    private const string NoTransferDetail = "no such transfer";
    private const string AnsweredDetail = "transfer already answered";
    private const string OwnerOfflineDetail = "the sender is offline";
    private const string OwnerGoneDetail = "the sender no longer has this file";
    private const string OwnerBusyDetail = "the sender is serving too many transfers";
    private const string NoAnswerDetail = "the sender did not answer";
    private const string ReaderGoneDetail = "the reader went away";
    private const string ShuttingDownDetail = "server shutting down";
    private const string TooLargeDetail = "files must be 1 TiB or smaller";
    private const string UnsatisfiableDetail = "range not satisfiable";
    private const string DefaultDeclineReason = "the sender declined";

    // An offer that declares no type is bytes of an unstated kind, which is precisely what this
    // media type means.
    private const string DefaultContentType = "application/octet-stream";

    // A decline's reason is echoed to the reader as the 410's detail: a short phrase, and nothing
    // a client can make longer than an error detail.
    private const int MaxReasonLength = 200;

    private const int TransferIdMaxDigits = 19;
    private const int CopyBufferSize = 64 * 1024;

    // A static class cannot be a type argument, so the log category is named rather than taken
    // from ILogger<T>.
    private const string LogCategory = "Vorcall.Server.Api.StreamsEndpoints";

    public static void Map(WebApplication app)
    {
        var options = app.Services.GetRequiredService<StreamOptions>();
        if (!options.Enabled)
        {
            app.Services.GetRequiredService<ILoggerFactory>().CreateLogger(LogCategory)
                .LogInformation("streamed files disabled: Vorcall:StreamsEnabled is false");
            return;
        }

        // An offer is an upload without the bytes — the same composer action, writing the same
        // kind of row — so it is charged to the upload budget rather than given one of its own.
        // The push and the decline are answers to this server's own requests, whose count the
        // per-owner transfer cap already bounds.
        app.MapPost("/api/streams", OfferAsync).RequireAuthorization().RequireRateLimiting(AttachmentsEndpoints.RateLimitPolicy);
        app.MapGet("/api/streams/{id:long}", GetAsync).RequireAuthorization();
        app.MapPost("/api/streams/{id:long}/chunks", PushAsync).RequireAuthorization();
        app.MapPost("/api/streams/{id:long}/decline", Decline).RequireAuthorization();
    }

    // POST /api/streams?channel=<id>, the body a StreamOffer -> 201 StreamedFile. Nothing is
    // uploaded: the row is the offer, and the bytes stay where they are.
    private static async Task<IResult> OfferAsync(
        HttpContext context,
        string? channel,
        StreamDirectory streams,
        ConnectionRegistry registry)
    {
        // An offer has to name the channel it is meant for: that is what the link check later
        // compares a SendMessage against.
        if (!Validation.TryParseChannelId(channel, out var channelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, ChannelField);
        }

        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // A hidden channel and a missing one answer alike, as everywhere else.
        if (registry.ChannelOf(channelId) is not { } info || !registry.CanView(userId, channelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ViewChannel));
        }

        // A voice channel carries no messages, so nothing could ever link the offer.
        if (info.Kind == Data.ChannelKind.Voice)
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, ChannelField);
        }

        if (!registry.Has(userId, channelId, Perm.AttachFiles))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.AttachFiles));
        }

        var parsed = await ProtobufBody.ReadAsync(context, StreamOffer.Parser);
        if (parsed.Message is not { } offer)
        {
            return parsed.Failure;
        }

        // The rule an attachment's declared type follows: any media type, but a media type.
        var contentType = string.IsNullOrWhiteSpace(offer.ContentType)
            ? DefaultContentType
            : offer.ContentType.Trim().ToLowerInvariant();
        if (!AttachmentsOptions.IsValidMediaType(contentType))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.MalformedTypeDetail);
        }

        if (offer.Size <= 0)
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, SizeField);
        }

        if (offer.Size > StreamOptions.MaxFileBytes)
        {
            return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
        }

        var streamed = await streams.OfferAsync(
            userId,
            channelId,
            AttachmentStore.SanitizeFileName(offer.FileName),
            contentType,
            offer.Size,
            context.RequestAborted);
        return ProtobufBody.Proto(streamed, StatusCodes.Status201Created);
    }

    // GET /api/streams/{id} -> the bytes, Range supported, straight from the owner's client. 409
    // while the owner is offline, 410 when it declined or its account is gone, 504 when it never
    // answered, 503 when it already serves as many transfers as it may.
    private static async Task<IResult> GetAsync(
        HttpContext context,
        long id,
        StreamDirectory streams,
        StreamRegistry transfers,
        StreamOptions options,
        ConnectionRegistry registry,
        IHostApplicationLifetime lifetime,
        ILoggerFactory loggers)
    {
        if (await streams.FindAsync(id, context.RequestAborted) is not { } row)
        {
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Before a message names it, an offer belongs to whoever made it; after, it belongs to the
        // channel that message is in, which the owner may have lost sight of since.
        if (row.MessageId is null)
        {
            if (row.OwnerId != userId)
            {
                return ProtobufBody.Fail(StatusCodes.Status403Forbidden, "not yours");
            }
        }
        else if (!registry.CanView(userId, row.ChannelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ViewChannel));
        }

        if (!TryResolveRange(context.Request, row.Size, out var offset, out var length, out var partial))
        {
            context.Response.Headers.ContentRange = new ContentRangeHeaderValue(row.Size).ToString();
            return ProtobufBody.Fail(StatusCodes.Status416RangeNotSatisfiable, UnsatisfiableDetail);
        }

        // An owner whose account is gone can never come back for the file.
        if (row.OwnerId is not { } ownerId)
        {
            return ProtobufBody.Fail(StatusCodes.Status410Gone, OwnerGoneDetail);
        }

        if (!registry.IsOnline(ownerId))
        {
            return ProtobufBody.Fail(StatusCodes.Status409Conflict, OwnerOfflineDetail);
        }

        if (transfers.TryRegister(ownerId, id, offset, length) is not { } transfer)
        {
            return ProtobufBody.Fail(StatusCodes.Status503ServiceUnavailable, OwnerBusyDetail);
        }

        var logger = loggers.CreateLogger(LogCategory);
        var request = new ServerFrame
        {
            StreamRequest = new StreamRequest { StreamId = id, TransferId = transfer.Id, Offset = offset, Length = length },
        };

        // Resolved by id at the moment of sending, never from a connection held earlier: the
        // account's live socket can have been replaced since IsOnline answered. A request the
        // outbox refused closes the owner's session as a slow consumer, which for this reader is
        // an owner gone offline, not a fault of the server's.
        if (registry.SendTo(ownerId, request) != SendToOutcome.Sent)
        {
            transfers.Abandon(transfer);
            return ProtobufBody.Fail(StatusCodes.Status409Conflict, OwnerOfflineDetail);
        }

        using var waiting = CancellationTokenSource.CreateLinkedTokenSource(context.RequestAborted, lifetime.ApplicationStopping);
        TransferAnswer answer;
        try
        {
            answer = await transfer.Answered.WaitAsync(options.SenderTimeout, waiting.Token);
        }
        catch (TimeoutException)
        {
            transfers.Abandon(transfer);
            logger.LogDebug(
                "Transfer {TransferId} of stream {StreamId}: user {OwnerId} did not answer within {Timeout}",
                transfer.Id,
                id,
                ownerId,
                options.SenderTimeout);
            return ProtobufBody.Fail(StatusCodes.Status504GatewayTimeout, NoAnswerDetail);
        }
        catch (OperationCanceledException)
        {
            // Either the reader hung up, and there is nobody to answer, or the host is stopping.
            transfers.Abandon(transfer);
            return lifetime.ApplicationStopping.IsCancellationRequested
                ? ProtobufBody.Fail(StatusCodes.Status503ServiceUnavailable, ShuttingDownDetail)
                : Results.Empty;
        }

        switch (answer)
        {
            case TransferAnswer.Declined:
                return ProtobufBody.Fail(StatusCodes.Status410Gone, transfer.DeclineReason);
            case TransferAnswer.OwnerOffline:
                return ProtobufBody.Fail(StatusCodes.Status409Conflict, OwnerOfflineDetail);
        }

        logger.LogDebug(
            "Transfer {TransferId} of stream {StreamId} claimed by user {OwnerId}: bytes {Offset}+{Length} of {Size} for user {UserId}",
            transfer.Id,
            id,
            ownerId,
            offset,
            length,
            row.Size,
            userId);
        return new ProxyResult(transfer, transfers, row.ContentType, row.Size, offset, length, partial, lifetime.ApplicationStopping);
    }

    // POST /api/streams/{id}/chunks?transfer=<n>, the body the bytes of the range asked for -> 204.
    // The owner's half of a transfer: the body is copied into the reader's response as it comes
    // and nowhere else. 404 for a transfer that is not this owner's, this stream's or anybody's,
    // 409 for one already pushed, 410 when the reader left first, 400 when the body ends short.
    private static async Task<IResult> PushAsync(
        HttpContext context,
        long id,
        string? transfer,
        StreamRegistry transfers,
        IHostApplicationLifetime lifetime,
        ILoggerFactory loggers)
    {
        if (!TryParseTransferId(transfer, out var transferId))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, TransferField);
        }

        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        var claim = transfers.Claim(transferId, userId, id);
        switch (claim.Status)
        {
            case AnswerStatus.Unknown:
                return ProtobufBody.Fail(StatusCodes.Status404NotFound, NoTransferDetail);
            case AnswerStatus.AlreadyClaimed:
                return ProtobufBody.Fail(StatusCodes.Status409Conflict, AnsweredDetail);
        }

        var pending = claim.Transfer!;

        // Kestrel's default body limit is 30 MB, and a range can be the whole of a file that was
        // too large to be an attachment. Exactly the range: nothing legitimate is longer, and the
        // copy below stops there regardless.
        if (context.Features.Get<IHttpMaxRequestBodySizeFeature>() is { IsReadOnly: false } bodySize)
        {
            bodySize.MaxRequestBodySize = pending.Length;
        }

        var logger = loggers.CreateLogger(LogCategory);
        using var copying = CancellationTokenSource.CreateLinkedTokenSource(
            pending.Cancellation,
            context.RequestAborted,
            lifetime.ApplicationStopping);

        Stream body;
        try
        {
            body = await pending.Body.WaitAsync(copying.Token);
        }
        catch (OperationCanceledException)
        {
            // Completed even though nothing was copied: a reader still on its way to handing the
            // body over would otherwise wait on a push that has already returned.
            transfers.Complete(pending, whole: false);
            return ProtobufBody.Fail(StatusCodes.Status410Gone, ReaderGoneDetail);
        }

        var copied = await CopyAsync(context.Request.Body, body, pending.Length, copying.Token);
        var whole = copied == pending.Length;
        transfers.Complete(pending, whole);

        logger.LogDebug(
            "Transfer {TransferId} of stream {StreamId}: user {UserId} pushed {Copied} of {Length} bytes",
            pending.Id,
            id,
            userId,
            copied,
            pending.Length);

        if (whole)
        {
            return Results.NoContent();
        }

        return pending.Cancellation.IsCancellationRequested
            ? ProtobufBody.Fail(StatusCodes.Status410Gone, ReaderGoneDetail)
            : ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.TruncatedDetail);
    }

    // POST /api/streams/{id}/decline?transfer=<n>&reason=<phrase> -> 204; the reader's GET answers
    // 410 carrying the reason.
    private static IResult Decline(
        HttpContext context,
        long id,
        string? transfer,
        string? reason,
        StreamRegistry transfers)
    {
        if (!TryParseTransferId(transfer, out var transferId))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, TransferField);
        }

        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        return transfers.Decline(transferId, userId, id, SanitizeReason(reason)) switch
        {
            AnswerStatus.Unknown => ProtobufBody.Fail(StatusCodes.Status404NotFound, NoTransferDetail),
            AnswerStatus.AlreadyClaimed => ProtobufBody.Fail(StatusCodes.Status409Conflict, AnsweredDetail),
            _ => Results.NoContent(),
        };
    }

    // At most length bytes from the owner's body into the reader's response, one buffer at a time.
    // Returns the bytes that went through: a body that ends or breaks, a response nobody reads any
    // more, or a cancellation stops the copy where it stands, and the caller reads the shortfall.
    private static async Task<long> CopyAsync(Stream source, Stream destination, long length, CancellationToken ct)
    {
        var buffer = ArrayPool<byte>.Shared.Rent(CopyBufferSize);
        long copied = 0;
        try
        {
            while (copied < length)
            {
                var wanted = (int)Math.Min(buffer.Length, length - copied);
                var read = await source.ReadAsync(buffer.AsMemory(0, wanted), ct);
                if (read == 0)
                {
                    break;
                }

                await destination.WriteAsync(buffer.AsMemory(0, read), ct);
                copied += read;
            }
        }
        catch (Exception ex) when (ex is IOException or OperationCanceledException or ObjectDisposedException or InvalidOperationException)
        {
            // Kestrel's request body and the other request's response stream between them: a
            // reset, a body over its limit, a write after the reader's response ended.
        }
        finally
        {
            ArrayPool<byte>.Shared.Return(buffer);
        }

        return copied;
    }

    // One range in bytes, as RFC 9110 § 14 and Results.File have it: no header, a malformed one,
    // another unit or several ranges is the whole file; a single range that starts past the end
    // is unsatisfiable, one that ends past it is clamped. length is what the response promises.
    private static bool TryResolveRange(HttpRequest request, long size, out long offset, out long length, out bool partial)
    {
        offset = 0;
        length = size;
        partial = false;

        var header = request.Headers.Range.ToString();
        if (header.Length == 0
            || !RangeHeaderValue.TryParse(header, out var range)
            || !range.Unit.Equals("bytes", StringComparison.OrdinalIgnoreCase)
            || range.Ranges.Count != 1)
        {
            return true;
        }

        var item = range.Ranges.First();
        long start;
        long end;
        if (item.From is { } from)
        {
            if (from >= size)
            {
                return false;
            }

            start = from;
            end = item.To is { } to ? Math.Min(to, size - 1) : size - 1;
        }
        else if (item.To is { } suffix)
        {
            // bytes=-N: the last N bytes.
            if (suffix == 0)
            {
                return false;
            }

            start = Math.Max(0, size - suffix);
            end = size - 1;
        }
        else
        {
            return true;
        }

        offset = start;
        length = end - start + 1;
        partial = true;
        return true;
    }

    // Digits only, like a channel id in a query string: a sign, a space or a non-ASCII digit
    // names no transfer rather than being normalised into one.
    private static bool TryParseTransferId(string? raw, out long id)
    {
        id = 0;
        if (raw is null || raw.Length is 0 or > TransferIdMaxDigits)
        {
            return false;
        }

        foreach (var c in raw)
        {
            if (c is < '0' or > '9')
            {
                return false;
            }
        }

        return long.TryParse(raw, NumberStyles.None, CultureInfo.InvariantCulture, out id) && id > 0;
    }

    private static string SanitizeReason(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw))
        {
            return DefaultDeclineReason;
        }

        var builder = new StringBuilder(MaxReasonLength);
        foreach (var rune in raw.Trim().EnumerateRunes())
        {
            if (Rune.IsControl(rune))
            {
                continue;
            }

            if (builder.Length + rune.Utf16SequenceLength > MaxReasonLength)
            {
                break;
            }

            builder.Append(rune);
        }

        var reason = builder.ToString().Trim();
        return reason.Length == 0 ? DefaultDeclineReason : reason;
    }

    // Modelled on ProtobufBody's result — status, type and length first, then the body — except
    // that the body is written by another request. Once the headers are out this hands the
    // response stream to the owner's push and waits for it. A push that ends short aborts the
    // connection: the Content-Length already promised is the one truth the reader has, and a
    // response that simply ended would read as a whole file.
    private sealed class ProxyResult(
        PendingTransfer transfer,
        StreamRegistry transfers,
        string contentType,
        long size,
        long offset,
        long length,
        bool partial,
        CancellationToken stopping) : IResult
    {
        public async Task ExecuteAsync(HttpContext httpContext)
        {
            var response = httpContext.Response;
            response.StatusCode = partial ? StatusCodes.Status206PartialContent : StatusCodes.Status200OK;
            response.ContentType = contentType;
            response.ContentLength = length;
            response.Headers.AcceptRanges = "bytes";
            if (partial)
            {
                response.Headers.ContentRange = new ContentRangeHeaderValue(offset, offset + length - 1, size).ToString();
            }

            // Live bytes off somebody else's disk, so nothing to cache; and, as for an
            // attachment, nothing a browser may sniff or render on this origin.
            response.Headers.CacheControl = "private, no-store";
            response.Headers.XContentTypeOptions = "nosniff";
            response.Headers.ContentDisposition = "attachment";

            using var waiting = CancellationTokenSource.CreateLinkedTokenSource(httpContext.RequestAborted, stopping);
            bool whole;
            try
            {
                // The headers go out before the first byte exists: the owner may take a moment to
                // reach the offset on its disk, and the reader should see its 206 rather than
                // silence.
                await response.StartAsync(waiting.Token);
                transfer.HandOver(response.Body);
                whole = await transfer.Done.WaitAsync(waiting.Token);
            }
            catch (Exception ex) when (ex is OperationCanceledException or IOException)
            {
                // The reader hung up or the host is stopping: the push has to stop too, rather
                // than pump gigabytes into a socket nobody reads.
                transfers.Abandon(transfer);
                httpContext.Abort();
                return;
            }

            if (!whole)
            {
                httpContext.Abort();
            }
        }
    }
}
