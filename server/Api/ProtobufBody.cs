using Google.Protobuf;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Api;

// Every REST body on this server is one protobuf message, capped at the same 16 KiB as a
// WebSocket frame.
public static class ProtobufBody
{
    public const string ContentType = "application/x-protobuf";
    public const int MaxBytes = 16 * 1024;

    private const int ReadBufferSize = 4 * 1024;

    // Message is null exactly when Failure carries the response to send instead.
    public readonly record struct Parsed<T>(T? Message, IResult Failure)
        where T : class, IMessage<T>;

    public static async Task<Parsed<T>> ReadAsync<T>(HttpContext context, MessageParser<T> parser)
        where T : class, IMessage<T>
    {
        // A declared length over the cap is refused before a single byte is read.
        if (context.Request.ContentLength > MaxBytes)
        {
            return TooLarge<T>();
        }

        using var assembled = new MemoryStream();
        var buffer = new byte[ReadBufferSize];
        while (true)
        {
            var read = await context.Request.Body.ReadAsync(buffer, context.RequestAborted);
            if (read == 0)
            {
                break;
            }

            // A chunked body has no declared length, so the cap is enforced again while reading.
            if (assembled.Length + read > MaxBytes)
            {
                return TooLarge<T>();
            }

            assembled.Write(buffer, 0, read);
        }

        try
        {
            return new Parsed<T>(parser.ParseFrom(assembled.ToArray()), Results.Empty);
        }
        catch (InvalidProtocolBufferException)
        {
            return new Parsed<T>(null, Fail(StatusCodes.Status400BadRequest, "malformed request body"));
        }
    }

    public static IResult Proto(IMessage message, int status = StatusCodes.Status200OK)
        => new ProtobufResult(message.ToByteArray(), status);

    public static IResult Fail(int status, string detail) => Proto(new ApiError { Detail = detail }, status);

    private static Parsed<T> TooLarge<T>()
        where T : class, IMessage<T>
        => new(null, Results.StatusCode(StatusCodes.Status413PayloadTooLarge));

    // Results.Bytes cannot carry a status code, and every failure body here needs one.
    private sealed class ProtobufResult(byte[] payload, int statusCode) : IResult
    {
        public async Task ExecuteAsync(HttpContext httpContext)
        {
            httpContext.Response.StatusCode = statusCode;
            httpContext.Response.ContentType = ContentType;
            httpContext.Response.ContentLength = payload.Length;
            await httpContext.Response.Body.WriteAsync(payload, httpContext.RequestAborted);
        }
    }
}
