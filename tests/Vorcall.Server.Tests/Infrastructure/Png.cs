using System.Buffers.Binary;
using System.IO.Compression;

namespace Vorcall.Server.Tests.Infrastructure;

// A real 1x1 PNG: the upload endpoint checks the magic number against the declared type, so a
// placeholder byte string would be refused as "body is not the declared image type".
internal static class Png
{
    public static byte[] Pixel()
    {
        var header = new byte[13];
        BinaryPrimitives.WriteUInt32BigEndian(header.AsSpan(0), 1);
        BinaryPrimitives.WriteUInt32BigEndian(header.AsSpan(4), 1);
        header[8] = 8;  // bit depth
        header[9] = 2;  // truecolour

        using var image = new MemoryStream();
        image.Write([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        WriteChunk(image, "IHDR"u8, header);
        WriteChunk(image, "IDAT"u8, Deflate([0x00, 0xFF, 0x00, 0x00]));
        WriteChunk(image, "IEND"u8, []);
        return image.ToArray();
    }

    private static byte[] Deflate(byte[] raw)
    {
        using var compressed = new MemoryStream();
        using (var zlib = new ZLibStream(compressed, CompressionLevel.Optimal, leaveOpen: true))
        {
            zlib.Write(raw);
        }

        return compressed.ToArray();
    }

    private static void WriteChunk(Stream target, ReadOnlySpan<byte> kind, ReadOnlySpan<byte> data)
    {
        var length = new byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(length, (uint)data.Length);
        target.Write(length);

        var body = new byte[kind.Length + data.Length];
        kind.CopyTo(body);
        data.CopyTo(body.AsSpan(kind.Length));
        target.Write(body);

        var crc = new byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(crc, Crc32(body));
        target.Write(crc);
    }

    private static uint Crc32(ReadOnlySpan<byte> data)
    {
        var crc = 0xFFFFFFFFu;
        foreach (var b in data)
        {
            crc ^= b;
            for (var bit = 0; bit < 8; bit++)
            {
                crc = (crc & 1) != 0 ? (crc >> 1) ^ 0xEDB88320u : crc >> 1;
            }
        }

        return crc ^ 0xFFFFFFFFu;
    }
}
