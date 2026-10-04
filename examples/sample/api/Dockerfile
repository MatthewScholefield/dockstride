FROM python:3-alpine
RUN addgroup -g 1000 app && adduser -D -u 1000 -G app app \
    && mkdir -p /app/content /data && chown -R app:app /app /data
WORKDIR /app
COPY --chown=app:app app.py migrate.py /app/
COPY --chown=app:app content /app/content
USER 1000:1000
EXPOSE 8000
HEALTHCHECK --interval=2s --timeout=2s --start-period=3s --retries=10 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=1)"
CMD ["python", "-u", "app.py"]
