import base64
import zlib
import json
import urllib.parse


def format_last_config(data):
    """
    Рекурсивно ищет поле last_config и парсит его из текстовой строки в JSON.
    Дополнительно разбивает поле config (внутри last_config) на список строк.
    """
    if isinstance(data, dict):
        for key, value in data.items():
            if key == "last_config" and isinstance(value, str):
                try:
                    # Превращаем экранированную строку в словарь
                    parsed_config = json.loads(value)

                    # Если внутри есть сырой текст конфига WireGuard (с переносами \n),
                    # разбиваем его на список строк, чтобы в JSON он выглядел красиво:
                    if "config" in parsed_config and isinstance(parsed_config["config"], str):
                        parsed_config["config"] = parsed_config["config"].strip().split("\n")

                    data[key] = parsed_config
                except json.JSONDecodeError:
                    pass  # Если не парсится как JSON, оставляем как есть
            else:
                format_last_config(value)
    elif isinstance(data, list):
        for item in data:
            format_last_config(item)
    return data


def parse_amnezia_link(link):
    try:
        # 1. Удаляем префикс 'vpn://'
        if link.startswith("vpn://"):
            encoded_data = link[6:]
        else:
            encoded_data = link

        # 2. Исправляем паддинг Base64
        missing_padding = len(encoded_data) % 4
        if missing_padding:
            encoded_data += '=' * (4 - missing_padding)

        # 3. Декодируем из Base64 (URL-safe)
        binary_data = base64.urlsafe_b64decode(encoded_data)

        # 4. Amnezia добавляет 4 байта заголовка в начале.
        # Попробуем пропустить их и распаковать.
        try:
            decompressed_data = zlib.decompress(binary_data)
        except zlib.error:
            decompressed_data = zlib.decompress(binary_data[4:])

        # 5. Преобразуем байты в строку и парсим JSON
        config_json = json.loads(decompressed_data.decode('utf-8'))

        # 6. Прогоняем через наш форматтер, чтобы улучшить вид last_config
        config_json = format_last_config(config_json)

        return config_json

    except Exception as e:
        return {"error": f"Не удалось распарсить ссылку: {str(e)}"}

vpn_link = "vpn://eNqlVl1zojwUvu-vyDB7165KAEVnvLDaLbrV2rquby0dJ0JsUzGwgB-10__-ThJEqLE7zspNfJ7nfOTkHMj7GQAAKI5PY0QoDiOlBh45xn7v6SqvUmpAQQuKtwR9R-tnqFzkhWj9rNQ-WXOi4zBT_ZNeUAtCGamW5CzaMNaQsgP1Cw4Kr6aU1Hg-UMrpR3O1eDxNL-uwrJpSa4uHNapaFRqlalUq4cFNU69AowzlEp6DWYEaVM2KJpO0eS5SBh5ltKOMfpQxjjGORzCN2y7j72bVF7S8vFvp28Z8OHfHrhueN95KfbVlbcfV2-EobATF4v3DXbd-3NeEBOLQCmZBLcAvhEFIVpM5fmNy0lmMm9fe5VtQhdOb29Gf0dWv7XhIjGvcX7_MRq_Nudm-scaEml_FDpbTncd_3c2LH8U9tMDMV1UrqKZRUMvlAtSkW_JQFE8cn84Imx7l3aYMtpWOYys1YCu6rVyk2IJQgaqlHIw2Ajay8ECVgXDnwMyiWhIMZkH9MAMr8ZlOQZZLXKftn-WSAGnfZ7kkTtrwGa6dxMtC8BDSDiH9EDIOIOR5_hq7ExJEjHsUuOBKBf4U99UTeK3GIIE8pZ524yBCnNJCmXTSOdidkZiEQ8VuAITulBGQ-BKd_2-J8_4VLh7bNMbhDDn4ybZpw3VDHEWgDna7KWrQtmmrNwB18K1_3-427h8mrd7gAnwbXDVve63kv23TfkhWKMY_8Ruog5N2adOOA-pAZ4sFoTw8X6MNqAODrQdquoKcN9lSY1YswYGe2FtMt-93m1pMvm9ym1rMaN_ZNrWY7b6dbWrb9LGPccgq0l9OPeKIPT3MUOv3xtvMu4ObeXn4q4vwSF9eXqHh766Fz9t-5fW5OPVKPo3qvB44ekEhdoX10D__bz0klctmcf0wM9c_uuOStrUW5vN11OnEpDd89UMDn6MbnVk3RLO3--w00u6-ALyhbXpF3cAnNAZ1kH9r1QxDh6wSfRxGJIoxjX9iHCCPrDCoA2iwDe57Yff6E92Qd5WRLeJl0uVapZzBgzTIZI5xMOFhhBIaWZ0fxgzmye3RaL5v5pPqs_cc4XCFw_xcnHRQGVfLKcXxBIkhyE119qUch4hGbEOTIPRjX-iWbsBeMx-yj8YiXvJPpVYpy2hpDZkBNKRyP4z5vYWVUipgWTm-N1kxxz6_sUk_Zkn5GX9K8WWu8qfAPJ5yBlKPucPIXDWkN8hPZ8LkSzdQcsKPs_zqSfhRXDxDSy9u_uXmrLg4ckISxElJW3hFHPy9Ui5BUytDWEp1NOL3PbXAnwwsrrl8llP4i_vH2cf_qQABXg"
print("ТЕКУЩЕЕ:")
print(json.dumps(parse_amnezia_link(vpn_link), indent=4, ensure_ascii=False))
vpn_link_normal = "vpn://AAALcXjatVZbT-M6EH7nV1TVvgHFceJc0LJSuR3aQumhCwtLVpWbuJCTxA1JWmAR__2M7bRNhfvASpu28vSbbzzj8Yzjt60GPM1gyksacZYXzf3GvcTE87aUJIs-P4B6HZSKMwPwJrEJMZFne7sGdkzbJrZBmjsaNhZsw3Zs5Ng2wrsYmYbrgCHW0k1Bx4gQ7BoeNoDuWhg-xNHSLUk3sO1i17NhdsN0LUQQ0gbTkaF_zRv429dxA724xEXwGNVPPbbt2aYd2BOH2BYyQZ7YIVryAoSCuo2BCIXBskLimo7pfNM6llnQasyNGmujhmzSdAOhsbSqlL7IjUN6bcTlRmm1Q5k37Gl1cmmednuGcnGW3k5tnU6V0KIcQZVOIlGCzTefC9iHyvPhv6-tPb-5s6RhRdMWXZ1nKp622uo8q-LpyqzG61Th_eX6qnusVlqHzI-Q9REiH6BuoCCrjkHNVClHa3DEqxTX4WG1fuzVwSpEr57RYRWktcZcpLmG0SSZPrNwFGWFUN4rXOlQS372ViEofH9fQAr5tZwpSCLGy06ofDyTG5LezSMvvRzMkuyVXD7Fofdyy8gkRvGhu82ub29eB-nj2cVBLRw1CUSzWH3LbRktw_5IyfJoPorZqyJu50dxlB3F_OHSTEmbtr3-qfc03gvHeDt3D4-G8fFV17y9CXCgcZfNxqup_jRy2U1qivsOL1k-oQH75fu8HYY5K4rGQWO5nD0Tg-K4PwTwy-Cqc9G-uhvB353Gl-HJ0WX_uPoPpAGsk5asx16B-6ll-rwbgI0lBKgn6V_K9AVkIuShARJUE0gYJCghkExhJDFLaEE4EzTtmQA6Yag_CEAp5tJ3Pyjl9NqW93lHuPzbfQ5uRPRiNKvRqkaiRvjeDxjLxT4OZuMkCtRGnPPe3d78uPMUpzhOf9CB0bvpoqeYtmOnuJgO08447Pe-5930-UBuIiseac5CZX0SzM7n19ns38A-PT9NsrbT_Z3z7wMzeLj-OadF4f133f5xM2f0RFi3VY92BqKGlk2505B96PMTHmbTiJdiA40WRvAzSMuwnH1LLFu4h5tIVJRQ6j3GMppEcyZSL_Jcq-DHaVH2acqq42RtqhotLWdVc5pOvS-zpRPoJZaNpJvqwKkf4tk0LwUsg1uhRbxqwU_lZzVzwfI5y9e7-VMbBWfau891L0wRtHzTiqC1hHxaToNpMpqLNEz5xldvMRtzVo6oOhTUjUCeCtppy5zyQjgfSQeCPguz5hrxfd1udfsUbJpy9juiu3DZxCuz9y11dqvrasgmdJaURxvtlrwiyKOsrJb3DwMuTZZaXshLjOHgFvZaciBWTavuqbJ8jQW8qDl5w1mruAWDg_YSkppHYcj44et1IQMs8xnbet_6H4egyRU"
print("Как должно быть:")
print(json.dumps(parse_amnezia_link(vpn_link_normal), indent=4, ensure_ascii=False))