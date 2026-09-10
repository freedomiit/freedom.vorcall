FROM mcr.microsoft.com/dotnet/sdk:10.0 AS build
ARG TARGETARCH
WORKDIR /src
COPY proto/ proto/
COPY server/Vorcall.Server.csproj server/
RUN RID=linux-x64; if [ "$TARGETARCH" = "arm64" ]; then RID=linux-arm64; fi; \
    dotnet restore server/Vorcall.Server.csproj -r $RID
COPY server/ server/
RUN RID=linux-x64; if [ "$TARGETARCH" = "arm64" ]; then RID=linux-arm64; fi; \
    dotnet publish server/Vorcall.Server.csproj -c Release -o /app/publish -r $RID --self-contained false

FROM mcr.microsoft.com/dotnet/aspnet:10.0 AS final
RUN apt-get update && apt-get install -y --no-install-recommends curl && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/publish .
ENV ASPNETCORE_URLS=http://0.0.0.0:5000
EXPOSE 5000/tcp
EXPOSE 5005/udp
ENTRYPOINT ["dotnet", "Vorcall.Server.dll"]
